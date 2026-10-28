use super::state::{ClientId, HostLock, HostState, SharedHost};
#[cfg(test)]
mod account_isolation_tests;
pub(super) mod capacity;
mod native_cleanup;
mod native_recovery;
mod owned_listener;
mod paired_close;
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
mod retained_resolution_tests;
mod retained_resume;
#[cfg(test)]
mod saved_account_tests;
#[cfg(test)]
mod startup_tests;
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

fn check_launch_account(launch: &chat::Launch) -> Result<(), ProtocolError> {
    crate::account::registry::authorize_spawn(&launch.recipe.environment, Some(launch.provider))
        .map(drop)
        .map_err(|failure| {
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
                false,
            )
        })
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
    // A conversation that starts or settles may take the messages left for it.
    if matches!(&update, Update::PromptAvailable)
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

fn current_session_snapshot(
    host: &SharedHost,
    session: Session,
    handle: Handle,
    execution_instance: uuid::Uuid,
) -> Result<ServerResponse, ProtocolError> {
    let response = session_snapshot(&session, &handle).map_err(unavailable)?;
    let state = host.lock().unwrap_or_else(|p| p.into_inner());
    if state.shutting_down
        || state.chats.get(&session.id).is_none_or(|current| {
            current.session != session
                || current.execution_instance != execution_instance
                || !current.handle.same_attachment(&handle)
        })
    {
        return Err(unavailable(
            "Native attachment changed while its exact snapshot was loading",
        ));
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
        unavailable(
            "No supported foreground agent is running in this terminal. Focus its CLI and retry.",
        )
    })?;
    if process.provider != provider {
        return Err(error(
            "The foreground provider changed; retry from the current terminal",
        ));
    }
    let hint = handle.observe_agent().map_err(unavailable)?;
    let fresh = crate::model::agent_recovery::fresh_omp_session(&process);
    if fresh.is_some()
        && !handle.agent_activity().is_some_and(|activity| {
            activity.provider == provider && activity.state == crate::agent::ActivityState::Idle
        })
    {
        return Err(unavailable(
            "Wait for the fresh agent to be idle before switching; an unsaved in-flight exchange will not be discarded.",
        ));
    }
    let public_id = crate::agent::observe::exact_session(&process).map_err(unavailable)?;
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
    }).ok_or_else(|| unavailable(match provider {
        Provider::Codex => "This terminal is not associated with a managed thread. Open New thread to use native and CLI views of one conversation.",
        Provider::Claude => "Claude has not published an exact live conversation yet. Retry once its live session is available; the terminal has not been stopped.",
        Provider::Omp => "OMP has not published an exact live conversation or fresh-session boundary for this terminal. The terminal has not been stopped; no recent history will be selected.",
    }))?;
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
        .map_err(unavailable)?;
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
    frozen.stop().map_err(unavailable)?;
    if !handle.is_completed()
        && let Err(failure) = handle.shutdown()
        && !handle.is_completed()
    {
        return Err(super::state::send_error(failure));
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    while !handle.is_completed() {
        if Instant::now() >= deadline {
            return Err(unavailable(
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
            return Err(unavailable(
                "Fresh chat handoff was cancelled; the source terminal is retained",
            ));
        }
        if handle.is_completed() {
            return Err(unavailable(
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
            return Err(unavailable(
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
    let directory = registry::directory(Kind::Native).map_err(unavailable)?;
    registry::recovery_candidate(&registry::socket(&directory, session.id), Kind::Native)
        // Unsupported retained protocols still own this allocation. Discovery
        // fences launch; only authenticated recovery may interpret its version.
        .map(|record| record.is_some())
        .map_err(unavailable)
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
) -> Result<ServerResponse, ProtocolError> {
    // A helper may have committed installation before host publication failed.
    // Recover its durable outbox through authenticated adoption before asking
    // an unexposed startup guard to retire it without a semantic consumer.
    let pending = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
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
        let response =
            native_recovery::restore(host, client, &pending.source).map_err(unavailable)?;
        pending.context.installed();
        retained_actor::forget_startup(host, &pending.source, &pending.context);
        return Ok(response);
    }
    // A failed pre-discovery helper still owns its exact cleanup context.
    retained_actor::reconcile_startup(host, &session).map_err(unavailable)?;
    // Reserve before provider/account initialization. Pending owners recovered above
    // use their own exact reservation through restore.
    let mut admission = capacity::Reservation::new_with_source(host, &session, source.as_ref())?;
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
        return native_recovery::restore_with_admission(host, client, &session, admission)
            .map_err(unavailable);
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
                return Err(unavailable(
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
                .map_err(unavailable)?;
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
            return Err(unavailable("Daemon is shutting down"));
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
    let allocator = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .proof()
        .clone();
    let owned =
        source.is_none() && session.claim_owned_codex_listener(&std::env::temp_dir(), &allocator);
    let owned_socket = owned.then(|| {
        session
            .owned_codex_listener
            .as_ref()
            .unwrap()
            .socket
            .clone()
    });
    // A server that stopped without cleaning up leaves its endpoint behind.
    // A listener still being served is joined; anything else is retired so
    // this conversation's own server starts again at the same path.
    let owned_socket = match owned_socket {
        Some(socket)
            if !owned_listener::served(&socket).map_err(|failure| {
                unavailable(format!("Cannot retire stopped provider socket: {failure}"))
            })? =>
        {
            Some(socket)
        }
        _ => None,
    };
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
    let process_launch = prepare_native_process_launch(
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
        chat::protocol::codex::create_with_listener(&process_launch, owned_socket)
    } else {
        chat::protocol::create(&process_launch)
    }
    .map_err(|message| {
        // Removal may have won after the early check while the adapter
        // was reading history. Do not hold the lifecycle lease across
        // those reads, but preserve the actionable account classification.
        check_launch_account(&session.launch)
            .err()
            .unwrap_or_else(|| error(message))
    })?;
    let remote = retained_actor::enabled(session.launch.provider, &spec.transport);
    let execution = if remote && execution.is_none() {
        Some(Arc::new(
            chat::protocol::ExecutionContext::capture(&spec.working_directory)
                .map_err(unavailable)?,
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
    spec.remove_unset_environment();
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
    let mut initial = adapter.initialize().map_err(error)?;
    let store = chat::runtime::preflight(&spec, &mut initial).map_err(error)?;
    if terminate_source && source.is_none() {
        return Err(error("Source termination requires an exact host proof"));
    }
    let source_handle = {
        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
        if state.shutting_down {
            return Err(unavailable("Daemon is shutting down"));
        }
        if state.chat_handoffs.contains_key(&session.id) {
            return Err(error(
                "This chat already returned to terminal; reattach and complete that handoff instead of starting another writer",
            ));
        }
        let recovering = if let Some(record) = state.chats.get(&session.id) {
            if record.returning_terminal {
                return Err(error(
                    "Retry the pending terminal handoff or explicitly close it before starting another native writer",
                ));
            }
            if record.handle.owns_work() {
                return Err(error(
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
            return Err(error("Chat is already starting"));
        }
        for record in state.chats.values() {
            let mut launch = record.session.launch.clone();
            launch.session_id = record.handle.session_id();
            if same_conversation(&launch, &session.launch) && record.handle.owns_work() {
                return Err(error(
                    "This exact conversation already has a daemon writer or unconfirmed backend work; attach to its existing chat",
                ));
            }
        }
        if state.chat_spawning.iter().any(|(id, pending)| {
            *id != session.id && same_conversation(&pending.launch, &session.launch)
        }) {
            return Err(error("This exact conversation is already starting"));
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
                    return Err(error(format!(
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
                let mut frozen = handle.prepare_upgrade().map_err(unavailable)?;
                if !frozen.only_agent_tree(current_process.pid) {
                    return Err(unavailable(
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
                return Err(unavailable(
                    "Daemon shutdown cancelled chat start; exact source/session evidence remains recoverable",
                ));
            }
        }
        spec.execution = execution.clone();
        if !matches!(spec.transport, chat::protocol::TransportSpec::History)
            && let Some(context) = &spec.execution
        {
            context
                .verify_directory(&spec.working_directory)
                .map_err(unavailable)?;
        }
        spec.remove_unset_environment();
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
                .map_err(unavailable)?;
            if let Some(token) = authority.token() {
                spec.environment
                    .insert(crate::agent_execution::AUTHORITY_ENV.into(), token.into());
            }
        }
        // Like the capability, transient: a cagents process the provider
        // starts must not take this conversation's account for the system
        // account.
        match crate::account::registry::selection_marker(|name| spec.environment.get(name).cloned())
        {
            Some(marker) => {
                spec.environment
                    .insert(crate::account::SELECTION_MARKER.into(), marker);
            }
            None => {
                spec.environment.remove(crate::account::SELECTION_MARKER);
            }
        }
        let execution_instance = uuid::Uuid::new_v4();
        if !matches!(
            spec.transport,
            chat::protocol::TransportSpec::History
                | chat::protocol::TransportSpec::UnixWebSocket { .. }
        ) {
            let _account = crate::account::registry::authorize_spawn(&spec.environment, None)
                .map_err(|error| unavailable(error.to_string()))?;
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
            if state.native_actor_startups.contains_key(&id) {
                return Err(unavailable(
                    "An exact retained startup still owns cleanup; no second helper was started",
                ));
            }
            state.native_actor_startups.insert(
                id,
                retained_actor::Startup {
                    source: startup_source.clone(),
                    requested: requested_source.clone(),
                    context,
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
                .unwrap_or_else(|| unavailable(message))
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
                    return Err(unavailable("Daemon shutdown cancelled fresh chat handoff"));
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
            handle.commit_native_installation().map_err(unavailable)?;
        }
        if remote {
            // A converted Claude source has been positively stopped above; it
            // remains a historical alias rather than a live helper-owned view.
            if source.is_some() {
                session.terminal = None;
                handle
                    .update_native_source(session.clone(), None, None)
                    .map_err(unavailable)?;
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
            session = retained_actor::publish_authenticated_source(
                host,
                &handle,
                session,
                &aliases,
                None,
                None,
                retained_actor::Publication::Install,
            )
            .map_err(unavailable)?;
        }
        let execution_instance = handle
            .actor_identity()
            .map_or(execution_instance, |identity| identity.instance);
        let _ = owner_instance.set(execution_instance);
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        if state.shutting_down {
            drop(state);
            let _ = handle.close();
            return Err(unavailable("Daemon shutdown cancelled chat start"));
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
        let response = session_snapshot(&record.session, &record.handle).map_err(unavailable)?;

        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
        if state.shutting_down
            || state.proof() != &proof
            || state.chat_spawning.get(&reservation_id) != Some(&reservation)
            || state
                .chats
                .get(&id)
                .is_some_and(|current| current.handle.owns_work())
        {
            drop(state);
            let _ = record.handle.close();
            return Err(unavailable(
                "Native startup ownership changed before attachment publication",
            ));
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
            if let Some(context) = &execution {
                retained_actor::forget_startup(host, &startup_source, context);
            }
            installed_handle.wake_retained_outcomes();
            retained_actor::wake(host, id);
        }
        Ok(response)
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
            owners: &owners,
            closing,
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
fn client_session(host: &SharedHost, session: Session) -> Session {
    client_session_in(host, session, &std::env::temp_dir())
}

fn client_session_in(host: &SharedHost, mut session: Session, root: &std::path::Path) -> Session {
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
                && owned.socket
                    == root
                        .join(format!("cagents-codex-{}", session.id))
                        .join("control.sock")
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

fn account_switch_unavailable(message: impl Into<String>) -> ProtocolError {
    ProtocolError::new(ProtocolErrorCode::AccountSwitchUnavailable, message, false)
}

/// Caller holds the control gate. This authenticates the current live actor,
/// not a cached UI snapshot, a pathname, or an account-selection generation.
fn account_switch_source(state: &HostState, source: &Session) -> Result<Handle, ProtocolError> {
    validate_proof(source.proof.as_ref(), state.proof())?;
    let record = state.chats.get(&source.id).ok_or_else(|| {
        account_switch_unavailable(
            "The original conversation is no longer hosted; reconnect before switching accounts",
        )
    })?;
    if record.session.launch.provider != source.launch.provider
        || record.session.launch.recipe != source.launch.recipe
        || record.session.terminal != source.terminal
        || record.handle.confirmed_session_id() != source.launch.session_id
        || source.launch.session_id.is_none()
        || record.returning_terminal
        || record.paired_terminal.is_some()
        || state.chat_handoffs.contains_key(&source.id)
        || state.chat_spawning.contains_key(&source.id)
        || record.handle.is_completed()
        || !record.handle.supports_account_switch()
    {
        return Err(account_switch_unavailable(
            "The original conversation cannot prove an exact account handoff; its owner is retained",
        ));
    }
    Ok(record.handle.clone())
}

/// Read current admissions separately from stopped-owner proof. The control
/// gate fences competing client mutations; producer callbacks may still update
/// ownership, so this census is repeated after retained evidence is inspected.
fn saved_account_admission(
    state: &HostState,
    source: &Session,
    replacement: &Session,
    members: &[TerminalId],
) -> Result<bool, ProtocolError> {
    let related = |id: TerminalId| id == replacement.id || members.contains(&id);
    let same = |session: &Session| same_conversation(&session.launch, &source.launch);
    let mut checked = members.to_vec();
    checked.push(replacement.id);
    if state.shutting_down
        || state.any_live(&checked)
        || state
            .chat_spawning
            .values()
            .any(|pending| related(pending.id) || same(pending))
        || state.native_actor_startups.values().any(|pending| {
            related(pending.source.id)
                || related(pending.requested.id)
                || same(&pending.source)
                || same(&pending.requested)
        })
        || state
            .native_closing
            .values()
            .any(|closing| related(closing.source().id) || same(closing.source()))
        || state
            .chat_handoffs
            .values()
            .any(|handoff| related(handoff.source.id) || same(&handoff.source))
        || state.chats.contains_key(&replacement.id)
        || state.terminals.contains_key(&replacement.id)
        || state.chats.values().any(|record| {
            let mut launch = record.session.launch.clone();
            launch.session_id = record.handle.confirmed_session_id().or(launch.session_id);
            same_conversation(&launch, &source.launch)
                && (!record.handle.is_completed()
                    || record.handle.owns_work()
                    || record.returning_terminal)
        })
    {
        return Err(account_switch_unavailable(
            "Saved recovery has a current owner or unsettled admission; no replacement was started",
        ));
    }
    for (&id, terminal) in &state.terminals {
        if related(id) && !terminal.handle.is_completed() {
            return Err(account_switch_unavailable(
                "A source-lineage terminal still owns work",
            ));
        }
        if !terminal.handle.is_completed() {
            let hint = terminal
                .session
                .resume_hint
                .as_deref()
                .and_then(crate::model::agent_recovery::observed_launch_from_hint);
            let recipe = hint.as_ref().unwrap_or(&terminal.session.launch);
            if recipe.arguments.iter().any(|argument| {
                Some(argument.as_str()) == source.launch.session_id.as_deref()
                    || argument.split_once('=').is_some_and(|(_, value)| {
                        Some(value) == source.launch.session_id.as_deref()
                    })
            }) {
                return Err(account_switch_unavailable(
                    "The saved conversation has a live CLI writer",
                ));
            }
        }
    }
    let completed = if let Some(record) = state.chats.get(&source.id) {
        validate_proof(source.proof.as_ref(), state.proof())?;
        if record.session.launch.provider != source.launch.provider
            || record.session.launch.recipe != source.launch.recipe
            || record.session.terminal != source.terminal
            || record.handle.confirmed_session_id() != source.launch.session_id
            || !record.handle.is_completed()
            || record.handle.owns_work()
            || record.returning_terminal
            || match (&source.terminal, &record.paired_terminal) {
                (None, None) => false,
                (Some(terminal), Some(pair)) => {
                    !pair.is_completed()
                        || !state.terminals.get(&terminal.id).is_some_and(|current| {
                            current.handle.same_worker(pair)
                                && matches_paired_terminal(
                                    &current.session,
                                    terminal,
                                    state.proof(),
                                )
                        })
                }
                _ => true,
            }
        {
            return Err(account_switch_unavailable(
                "The exact saved source has no positive completed-owner evidence",
            ));
        }
        true
    } else {
        false
    };
    capacity::require_available(state)
        .map_err(|failure| account_switch_unavailable(failure.message))?;
    Ok(completed)
}

/// Missing roster rows do not make orphaned legacy holders harmless. Inspect
/// every retained endpoint without adopting it or dropping malformed evidence.
fn account_recovery_retained_discovery(
    source: &Session,
    replacement: &Session,
    members: &[TerminalId],
    retired: &BTreeMap<TerminalId, crate::daemon::holder_registry::Record>,
    proof: &HostSessionProof,
) -> Result<(), ProtocolError> {
    use crate::daemon::holder_registry::{self as holders, Kind};
    let deadline = Instant::now() + Duration::from_secs(5);
    let related = |id: TerminalId| members.contains(&id) || id == replacement.id;
    let conflicts = |candidate: &Session, confirmed: Option<&str>| {
        related(candidate.id)
            || candidate.launch.provider == source.launch.provider
                && (same_conversation(&candidate.launch, &source.launch)
                    || confirmed.is_none()
                    || confirmed == source.launch.session_id.as_deref())
    };
    for kind in [Kind::Native, Kind::Terminal] {
        let directory = holders::directory(kind).map_err(account_switch_unavailable)?;
        for owner in holders::recovery_census(&directory, &proof.boot_id, kind)
            .map_err(account_switch_unavailable)?
        {
            if Instant::now() >= deadline {
                return Err(account_switch_unavailable(
                    "Retained ownership census timed out; retry after exact recovery",
                ));
            }
            if kind == Kind::Native && retired.get(&owner.terminal) == Some(&owner) {
                continue;
            }
            if kind == Kind::Native {
                let collision = if Handle::is_remote_record(&owner) {
                    let inspection = Handle::inspect_remote_until(&owner, proof.clone(), deadline)
                        .map_err(account_switch_unavailable)?;
                    conflicts(
                        &inspection.source,
                        inspection.state.confirmed_session.as_deref(),
                    ) || inspection.previous_sources.iter().any(|alias| {
                        conflicts(alias, inspection.state.confirmed_session.as_deref())
                    })
                } else {
                    let (inspection, _) = chat::native_holder::inspect(&owner, proof)
                        .map_err(account_switch_unavailable)?;
                    conflicts(&inspection.source, inspection.confirmed_thread.as_deref())
                        || inspection
                            .previous_sources
                            .iter()
                            .any(|alias| conflicts(alias, inspection.confirmed_thread.as_deref()))
                };
                if collision {
                    return Err(account_switch_unavailable(
                        "A retained native owner conflicts with saved recovery; restore and close its exact original",
                    ));
                }
            } else {
                let (inspection, _) = crate::terminal::inspect_holder(
                    &holders::socket(&directory, owner.terminal),
                    &owner,
                    proof,
                )
                .map_err(account_switch_unavailable)?;
                let recipe = inspection
                    .execution_recipe
                    .as_ref()
                    .unwrap_or(&inspection.source.launch);
                let conversation = crate::upgrade::exact_conversation(recipe);
                if related(owner.terminal)
                    || conversation.is_none_or(|(provider, id)| {
                        provider == source.launch.provider
                            && source.launch.session_id.as_deref() == Some(id.to_string().as_str())
                    })
                {
                    return Err(account_switch_unavailable(
                        "A retained terminal has matching or unknown conversation ownership",
                    ));
                }
            }
            if Instant::now() >= deadline {
                return Err(account_switch_unavailable(
                    "Retained ownership census exceeded its inspection deadline",
                ));
            }
        }
    }
    Ok(())
}

fn recover_saved_account(
    host: &SharedHost,
    client: ClientId,
    source: Session,
    replacement: Session,
    execution_environment: crate::terminal_ipc::ChatExecutionEnvironment,
) -> Result<ServerResponse, ProtocolError> {
    if replacement
        .launch
        .recipe
        .environment
        .contains_key(crate::account::routing::AUTOMATIC_MARKER)
    {
        return Err(error(
            "Saved account recovery must pin the selected account",
        ));
    }
    if !source.has_valid_native_continuity() {
        return Err(account_switch_unavailable(
            "The saved source has invalid continuity evidence",
        ));
    }
    check_launch_account(&replacement.launch)
        .map_err(|failure| account_switch_unavailable(failure.message))?;
    let registry = super::agent_execution::registry(host).map_err(account_switch_unavailable)?;
    let root = source
        .terminal
        .as_ref()
        .and_then(|terminal| terminal.recovery_origin);
    let members = registry
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .recovery_lineage_members(source.id, root)
        .map_err(account_switch_unavailable)?;
    let completed = saved_account_admission(
        &host.lock().unwrap_or_else(|p| p.into_inner()),
        &source,
        &replacement,
        &members,
    )?;
    let options = if completed {
        let handle = host
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .chats
            .get(&source.id)
            .map(|record| record.handle.clone())
            .ok_or_else(|| {
                account_switch_unavailable(
                    "The completed source changed before options were captured",
                )
            })?;
        let snapshot = handle.snapshot(None).ok_or_else(|| {
            account_switch_unavailable(
                "The completed source's confirmed conversation options are unavailable",
            )
        })?;
        Some(chat::runtime::AccountSwitchOptions::capture(&snapshot))
    } else {
        None
    };
    let retired = retained_resume::account_recovery_census(host, &source, &members)
        .map_err(account_switch_unavailable)?;
    let proof = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .proof()
        .clone();
    let mut closed = completed || retired.contains_key(&source.id);
    if !closed && source.launch.provider == Provider::Codex {
        closed = chat::native_holder::closed_source(&source, &proof)
            .map_err(account_switch_unavailable)?
            .is_some_and(|(_, paired)| paired);
    }
    if !closed {
        return Err(account_switch_unavailable(
            "The saved source has no authenticated completed owner or close receipt; restore and close its original before retrying",
        ));
    }
    account_recovery_retained_discovery(&source, &replacement, &members, &retired, &proof)?;
    let context = Arc::new(
        chat::protocol::ExecutionContext::capture(&replacement.launch.recipe.working_directory)
            .map_err(account_switch_unavailable)?,
    );
    let reservation = {
        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
        saved_account_admission(&state, &source, &replacement, &members)?;
        state.chat_spawning.insert(source.id, source.clone());
        AccountSwitchReservation {
            host: host.clone(),
            source: source.clone(),
        }
    };
    // Saved recovery deliberately reads only the selected account's existing
    // history. A removed source home supplies neither a transcript nor authority.
    // start_with_execution performs current target/directory/duplicate admission.
    registry
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        // The exclusive control gate plus the completed admission census
        // above excludes live family members. No HostState lock or IO runs
        // under this registry transaction; a saved recovery never forks.
        .succeed(replacement.id, source.id, root, |_| false)
        .map_err(account_switch_unavailable)?;
    drop(reservation);
    start_confirmed_account_recovery(
        host,
        client,
        source,
        replacement,
        execution_environment,
        context,
        options.as_ref(),
    )
}

fn recover_account(
    host: &SharedHost,
    client: ClientId,
    source: Session,
    replacement: Session,
    idle_generation: Option<u64>,
    execution_environment: crate::terminal_ipc::ChatExecutionEnvironment,
    explicit: bool,
) -> Result<ServerResponse, ProtocolError> {
    if !chat::is_account_recovery(&source, &replacement)
        || replacement.proof.is_some()
        || replacement.owned_codex_listener.is_some()
        || replacement.native_continuity.retains_provider()
    {
        return Err(error(
            "Account recovery must retain the exact provider, conversation and launch options",
        ));
    }
    if !explicit && idle_generation.is_none() {
        return recover_saved_account(host, client, source, replacement, execution_environment);
    }
    let automatic = idle_generation.is_some() && !explicit;
    if explicit
        && replacement
            .launch
            .recipe
            .environment
            .contains_key(crate::account::routing::AUTOMATIC_MARKER)
    {
        return Err(error(
            "An explicit account switch must pin the chosen account",
        ));
    }
    match check_launch_account(&source.launch) {
        Err(failure) if failure.code == ProtocolErrorCode::AccountRemoved => {}
        Err(failure) => return Err(failure),
        Ok(()) if automatic || explicit => {}
        Ok(()) => {
            return Err(error(
                "The original account is available; retry the saved conversation normally",
            ));
        }
    }
    if automatic
        && source
            .launch
            .recipe
            .environment
            .get(crate::account::routing::AUTOMATIC_MARKER)
            .map(String::as_str)
            != Some("1")
    {
        return Err(error("This conversation has an explicitly pinned account"));
    }
    // A selected target may have been removed or become unreadable since the
    // chooser was rendered. Refuse that already-known failure while the exact
    // healthy original still owns its conversation. Spawn reauthorizes later.
    check_launch_account(&replacement.launch)
        .map_err(|failure| account_switch_unavailable(failure.message))?;
    let (original, reservation) = {
        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
        let handle = account_switch_source(&state, &source)?;
        if state.chats.contains_key(&replacement.id)
            || state.terminals.contains_key(&replacement.id)
            || state.chat_spawning.contains_key(&replacement.id)
            || state.chat_handoffs.contains_key(&replacement.id)
        {
            return Err(account_switch_unavailable(
                "The replacement identity already has an owner",
            ));
        }
        // A different card is a distinct resident slot, even after its source
        // completes. Refuse capacity before ending the healthy source.
        capacity::require_available(&state)
            .map_err(|failure| account_switch_unavailable(failure.message))?;
        state.chat_spawning.insert(source.id, source.clone());
        (
            handle,
            AccountSwitchReservation {
                host: host.clone(),
                source: source.clone(),
            },
        )
    };
    // First observe the real actor, before any transcript scan. A supplied
    // generation is only a conditional fence and must still match this actor.
    let generation = match (
        idle_generation,
        original.account_switch_generation().map_err(unavailable)?,
    ) {
        (None, Some(current)) => current,
        (Some(expected), Some(current)) if expected == current => current,
        _ => {
            return Err(ProtocolError::new(
                ProtocolErrorCode::AccountSwitchBusy,
                "The original actor has unfinished work, controls, or a newer generation; wait for a newer idle observation",
                true,
            ));
        }
    };
    // Keep the original physical descriptor across the close. Never reacquire
    // the old pathname and silently launch in a replacement directory.
    let (identity, directory) = original
        .pinned_launch_directory()
        .map_err(account_switch_unavailable)?;
    let context = Arc::new(chat::protocol::ExecutionContext::new(Some(directory)));
    if context
        .directory_identity()
        .map_err(account_switch_unavailable)?
        != Some(identity)
    {
        return Err(account_switch_unavailable(
            "Original launch-directory identity changed",
        ));
    }
    context
        .verify_directory(std::path::Path::new(
            &source.launch.recipe.working_directory,
        ))
        .map_err(account_switch_unavailable)?;
    let target = &replacement.launch.recipe;
    super::account_lifecycle::ensure_launch_at_directory(
        host,
        &target.working_directory,
        &target.environment,
        &target.unset_environment,
        false,
        context.directory.as_deref(),
    )
    .map_err(|failure| account_switch_unavailable(failure.message))?;
    super::account_lifecycle::ensure_launch(
        host,
        &target.working_directory,
        &target.environment,
        &target.unset_environment,
        false,
    )
    .map_err(|failure| account_switch_unavailable(failure.message))?;
    crate::account::handoff::preflight(&source, &replacement)
        .map_err(account_switch_unavailable)?;
    // Recheck the exact reserved owner after the read-only physical preflight.
    {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        if state.chat_spawning.get(&source.id) != Some(&source)
            || !state.chats.get(&source.id).is_some_and(|record| {
                record.session.launch == source.launch
                    && record.handle.same_actor(&original)
                    && record.handle.confirmed_session_id() == source.launch.session_id
                    && !record.returning_terminal
                    && record.paired_terminal.is_none()
            })
        {
            return Err(account_switch_unavailable(
                "The original conversation changed during account preflight",
            ));
        }
    }
    // None is the actor's positive pre-effect refusal. Any acknowledgement
    // error may already have stopped it and must never become retryable Busy.
    let options = original.close_for_account_switch(generation).map_err(unavailable)?
        .ok_or_else(|| ProtocolError::new(ProtocolErrorCode::AccountSwitchBusy,
            "The original actor changed before handoff admission; wait for a newer idle observation", true))?;
    crate::account::handoff::prepare(&source, &replacement).map_err(unavailable)?;
    let id = replacement.id;
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
    drop(reservation);
    start_confirmed_account_recovery(
        host,
        client,
        source,
        replacement,
        execution_environment,
        context,
        Some(&options),
    )
}

fn start_confirmed_account_recovery(
    host: &SharedHost,
    client: ClientId,
    source: Session,
    replacement: Session,
    execution_environment: crate::terminal_ipc::ChatExecutionEnvironment,
    context: Arc<chat::protocol::ExecutionContext>,
    options: Option<&chat::runtime::AccountSwitchOptions>,
) -> Result<ServerResponse, ProtocolError> {
    let id = replacement.id;
    start_with_execution(
        host,
        client,
        replacement,
        None,
        false,
        None,
        execution_environment,
        Some(context),
        None,
    )?;
    let handle = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .chats
        .get(&id)
        .ok_or_else(|| unavailable("The account recovery candidate disappeared"))?
        .handle
        .clone();
    let confirmation = confirm_account_conversation(
        &handle,
        source.launch.session_id.as_deref().unwrap(),
        Instant::now() + Duration::from_secs(15),
        || crate::daemon::termination_requested(),
    );
    let response = confirmation.map_err(unavailable).and_then(|()| {
        if let Some(options) = options {
            handle
                .restore_account_options(options, Instant::now() + Duration::from_secs(15), || {
                    crate::daemon::termination_requested()
                })
                .map_err(unavailable)?;
        }
        handle.account_switch_notice().map_err(unavailable)?;
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

fn confirm_account_conversation(
    handle: &Handle,
    expected: &str,
    deadline: Instant,
    cancelled: impl Fn() -> bool,
) -> Result<(), String> {
    loop {
        if cancelled() {
            return Err(
                "Account recovery was cancelled; the saved conversation is unchanged".into(),
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
            // Session identity can precede initial history/skills/setup. Never
            // publish the saved rebind before that setup has positively finished.
            if handle.update_activity().1 {
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
            let proof = host
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .proof()
                .clone();
            if !source.has_valid_native_continuity() || source.launch.session_id.is_none() {
                return Err(error("Saved conversation has invalid continuity evidence"));
            }
            // Classify retained Claude execution before generic discovery:
            // an exited helper may still await its original parent's wait.
            // Errors/unproved exit never fall back to treating it as live.
            let retirement = if source.launch.provider == Provider::Claude {
                retained_resume::inspect_resume_retirement(host, &source).map_err(unavailable)?
            } else {
                None
            };
            // A present owner must be restored, not replaced. Missing evidence
            // is never proof of a completed close.
            if retirement.is_none() && has_native_owner(&source)? {
                return Ok(ServerResponse::ChatResumePrepared { session: source });
            }
            // A kernel boot change proves every process from the saved boot
            // is gone. Unlike missing runtime files within the same boot, it
            // authorizes an exact disk resume without its ephemeral receipt.
            let previous_boot = source.proof.as_ref().is_some_and(|previous| {
                previous.is_complete() && previous.boot_id != proof.boot_id
            });
            let closed = if previous_boot {
                true
            } else if source.launch.provider == Provider::Claude {
                retained_resume::prepare_resume_verified(host, &source, retirement)
                    .map_err(unavailable)?
            } else {
                chat::native_holder::closed_source(&source, &proof)
                    .map_err(unavailable)?
                    .is_some_and(|(_, paired_closed)| paired_closed)
            };
            if !closed {
                return Err(unavailable(
                    "The old provider owner has no confirmed close receipt. Restore its original pane and close it before reopening; no replacement was started.",
                ));
            }
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
                    .map_err(unavailable)?;
            }
            Ok(ServerResponse::ChatResumePrepared { session })
        }
        ClientRequest::ChatAccountSwitchGeneration { source } => {
            let handle = {
                let state = host.lock().unwrap_or_else(|p| p.into_inner());
                account_switch_source(&state, &source)?
            };
            let generation = handle.account_switch_generation().map_err(unavailable)?;
            let state = host.lock().unwrap_or_else(|p| p.into_inner());
            let current = account_switch_source(&state, &source)?;
            if !current.same_actor(&handle) {
                return Err(account_switch_unavailable(
                    "The original actor changed during readiness observation",
                ));
            }
            Ok(ServerResponse::ChatAccountSwitchGeneration { generation })
        }
        ClientRequest::ChatRecoverAccount {
            source,
            replacement,
            idle_generation,
            execution_environment,
            explicit,
        } => recover_account(
            host,
            client,
            source,
            replacement,
            idle_generation,
            execution_environment,
            explicit,
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
                .filter(|record| record.handle.owns_work() || record.returning_terminal)
                .map(|record| {
                    let mut session = record.session.clone();
                    session.launch.session_id = record.handle.session_id();
                    session.native_continuity = record.handle.native_continuity();
                    session
                })
                .collect::<Vec<_>>();
            sessions.extend(state.chat_spawning.values().map(|pending| {
                let mut session = pending.clone();
                // Inventory describes this daemon's pending attachment. Keep
                // the reservation's exact caller token unchanged for its CAS.
                session.proof = Some(state.proof().clone());
                session
            }));
            Ok(ServerResponse::ChatListed { sessions })
        }
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
            start(
                host,
                client,
                session,
                source,
                terminate_source,
                None,
                execution_environment,
            )
        }
        ClientRequest::ChatAttach { session } => {
            if let Some(response) = upgrade_resolution::attach(host, client, &session)? {
                return Ok(response);
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
                    return Err(unavailable(
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
                return Err(unavailable(
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
        } => {
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
                });
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
                .presentation(chat_id, after_generation)
                .map_err(unavailable)?
                .unwrap_or(ServerResponse::ChatSnapshot {
                    chat_id,
                    snapshot: None,
                });
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
    owners: &'a paired_close::Owners,
    closing: &'a capacity::Reservation,
}

impl TerminalViewReservation<'_> {
    pub(super) fn authorizes(&self, state: &HostState, terminal_id: TerminalId) -> bool {
        self.owners.target.source.id == terminal_id
            && self.owners.require_locked(state, self.closing).is_ok()
            && state.chats.get(&terminal_id).is_some_and(|record| {
                record.returning_terminal
                    && record.session.launch.provider == Provider::Codex
                    && record.session.terminal.is_none()
                    && record.paired_terminal.is_none()
            })
    }
}

/// A close capability captures one hosted attachment, not just its reusable ID.
/// Execution callers authenticate their model origin before obtaining it; UI
/// callers must pass the ordinary subscriber admission above.
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
        capacity::Reservation::closing(host, &mut state, &source)?
    };
    super::agent_execution::preflight_retirement(host, &source).map_err(unavailable)?;
    target.require_current(host)?;
    handle.close_for_resume().map_err(unavailable)?;
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
        if !record.handle.owns_work() {
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
        spec.environment = sourceless_environment(daemon, selected.clone(), spec.environment);
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
    spec.remove_unset_environment();
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
            "Native execution environment cannot survive daemon upgrade without losing selected credentials or account settings. Align the daemon configuration or finish and close this conversation before upgrading; no source was stopped."
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
mod tests {
    use super::*;
    use crate::{
        chat::{
            Command, Event,
            protocol::{Adapter, ProcessSpec, Step},
        },
        model::{BootId, HostIncarnation, HostSessionId, LaunchRecipe},
    };
    use serde_json::{Value, json};

    #[test]
    fn chat_read_reports_remote_endpoint_failure_without_retiring_owned_chat() {
        let (handle, _other_attachment) = Handle::fixture_remote_attachment_pair();
        let identity = handle.actor_identity().unwrap();
        let proof = HostSessionProof {
            host_session_id: "presentation-host".into(),
            boot_id: "presentation-boot".into(),
            host_incarnation: "presentation-incarnation".into(),
        };
        let source = Session {
            id: identity.record.terminal,
            launch: chat::Launch {
                provider: Provider::Claude,
                recipe: LaunchRecipe::shell("/".into()).unwrap(),
                session_id: Some(uuid::Uuid::new_v4().to_string()),
            },
            proof: Some(proof.clone()),
            terminal: None,
            owned_codex_listener: None,
            native_continuity: chat::NativeContinuity::RetainedClaudeActor { protocol: 2 },
        };
        let mut state = HostState::new(proof);
        state.register_client(1);
        state.chats.insert(
            source.id,
            Record {
                execution_instance: identity.instance,
                console_recipe: None,
                paired_terminal: None,
                session: source.clone(),
                handle: handle.clone(),
                source_process: None,
                subscribers: BTreeMap::from([(1, true)]),
                observations: BTreeMap::new(),
                completion_observed: false,
                returning_terminal: false,
            },
        );
        let host = Arc::new(HostLock::new(state));
        for after_generation in [None, Some(1)] {
            let failure = process(
                &host,
                1,
                ClientRequest::ChatRead {
                    chat_id: source.id,
                    after_generation,
                },
            )
            .unwrap_err();
            assert_eq!(failure.code, ProtocolErrorCode::TerminalUnavailable);
            assert!(!failure.message.is_empty());
            let state = host.lock().unwrap();
            let retained = &state.chats[&source.id];
            assert_eq!(retained.session, source);
            assert!(retained.handle.same_attachment(&handle));
            assert!(retained.handle.owns_work());
            assert!(!retained.handle.is_completed());
            assert!(state.native_closing.is_empty());
            assert!(state.chat_spawning.is_empty());
        }
    }

    #[test]
    fn native_constructor_environment_is_private_once_and_preserves_precedence_and_removals() {
        let root = tempfile::tempdir().unwrap();
        for provider in [Provider::Claude, Provider::Codex, Provider::Omp] {
            let mut launch = chat::Launch {
                provider,
                session_id: None,
                recipe: LaunchRecipe {
                    program: "private-fixture".into(),
                    arguments: vec![],
                    working_directory: root.path().into(),
                    environment: [
                        ("HOME".into(), root.path().to_str().unwrap().into()),
                        ("PATH".into(), "/recipe/bin".into()),
                    ]
                    .into(),
                    unset_environment: ["HTTPS_PROXY".into()].into(),
                },
            };
            let issued = launch.clone();
            let daemon = [
                ("PATH".into(), "/daemon/bin".into()),
                ("HTTPS_PROXY".into(), "daemon-proxy".into()),
            ]
            .into();
            let selected = [("PATH".into(), "/selected/bin".into())].into();
            let prepared =
                prepare_native_process_launch(&launch, Some(daemon), &selected, None).unwrap();
            assert_eq!(prepared.recipe.environment["PATH"], "/recipe/bin");
            assert!(!prepared.recipe.environment.contains_key("HTTPS_PROXY"));
            assert_eq!(launch, issued);
            let source = [
                ("HOME".into(), root.path().to_str().unwrap().into()),
                ("PATH".into(), "/source/bin".into()),
            ]
            .into();
            let prepared =
                prepare_native_process_launch(&launch, None, &BTreeMap::new(), Some(&source))
                    .unwrap();
            assert_eq!(prepared.recipe.environment, source);
            let spec = chat::protocol::ProcessSpec::from(&prepared.recipe);
            assert_eq!(spec.environment, prepared.recipe.environment);
            assert_eq!(spec.unset_environment, prepared.recipe.unset_environment);
            assert_eq!(launch, issued);
            launch.recipe.environment.remove("PATH");
            assert!(
                prepare_native_process_launch(
                    &launch,
                    Some(BTreeMap::new()),
                    &[(
                        "UNRELATED_PRIVATE_FIXTURE_KEY".into(),
                        "do-not-admit".into()
                    )]
                    .into(),
                    None
                )
                .is_err()
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn native_constructor_history_uses_effective_source_selected_and_unset_home_without_reading_issued_home()
     {
        use std::{
            io::Read,
            os::fd::{AsRawFd, FromRawFd},
        };
        const CONVERSATION: &str = "12345678-1234-4234-8234-123456789abc";
        for mode in ["source", "selected", "unset"] {
            let root = tempfile::tempdir().unwrap();
            let cwd = root.path().join("project");
            std::fs::create_dir(&cwd).unwrap();
            let cwd = cwd.canonicalize().unwrap();
            let project: String = cwd
                .to_str()
                .unwrap()
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
                .collect();
            let issued_home = root.path().join("issued-home");
            let actual_home = root.path().join("actual-home");
            let issued_config = issued_home.join(".claude");
            let actual_config = actual_home.join(".claude");
            let mut issued_file = None;
            for (config, body) in [
                (&issued_config, "issued-A-body"),
                (&actual_config, "effective-B-body"),
            ] {
                let path = config
                    .join("projects")
                    .join(&project)
                    .join(format!("{CONVERSATION}.jsonl"));
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(&path, format!("{}\n{}\n",
                    json!({"type":"user","uuid":"private-prompt","parentUuid":null,"sessionId":CONVERSATION,"cwd":cwd,"message":{"role":"user","content":body}}),
                    json!({"type":"custom-title","sessionId":CONVERSATION,"customTitle":format!("{body} title")}))).unwrap();
                if config == &issued_config {
                    issued_file = Some(path);
                }
            }
            let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
            assert!(fd >= 0);
            let mut watcher = unsafe { std::fs::File::from_raw_fd(fd) };
            let path = std::ffi::CString::new(issued_file.unwrap().as_os_str().as_encoded_bytes())
                .unwrap();
            assert!(
                unsafe {
                    libc::inotify_add_watch(
                        watcher.as_raw_fd(),
                        path.as_ptr(),
                        libc::IN_OPEN | libc::IN_ACCESS,
                    )
                } >= 0
            );
            let mut launch = chat::Launch {
                provider: Provider::Claude,
                session_id: Some(CONVERSATION.into()),
                recipe: LaunchRecipe {
                    program: "private-claude-not-executed".into(),
                    arguments: vec!["--resume".into(), CONVERSATION.into()],
                    working_directory: cwd,
                    environment: [
                        ("HOME".into(), issued_home.to_str().unwrap().into()),
                        (
                            "CLAUDE_CONFIG_DIR".into(),
                            issued_config.to_str().unwrap().into(),
                        ),
                    ]
                    .into(),
                    unset_environment: Default::default(),
                },
            };
            let source = [
                ("HOME".into(), actual_home.to_str().unwrap().into()),
                (
                    "CLAUDE_CONFIG_DIR".into(),
                    actual_config.to_str().unwrap().into(),
                ),
            ]
            .into();
            let selected = if mode == "selected" {
                launch.recipe.environment.remove("CLAUDE_CONFIG_DIR");
                [(
                    "CLAUDE_CONFIG_DIR".into(),
                    actual_config.to_str().unwrap().into(),
                )]
                .into()
            } else {
                BTreeMap::new()
            };
            if mode == "unset" {
                launch
                    .recipe
                    .environment
                    .insert("HOME".into(), actual_home.to_str().unwrap().into());
                launch
                    .recipe
                    .unset_environment
                    .insert("CLAUDE_CONFIG_DIR".into());
            }
            let issued = launch.clone();
            let prepared = prepare_native_process_launch(
                &launch,
                (mode != "source").then(BTreeMap::new),
                &selected,
                (mode == "source").then_some(&source),
            )
            .unwrap();
            assert_eq!(
                Provider::Claude.session_root(&prepared.recipe),
                Some(actual_config.clone())
            );
            let (spec, mut adapter) = chat::protocol::create(&prepared).unwrap();
            assert_eq!(spec.environment, prepared.recipe.environment);
            assert_eq!(spec.unset_environment, prepared.recipe.unset_environment);
            let initial = adapter.initialize().unwrap();
            let items: Vec<_> = initial
                .events
                .iter()
                .filter_map(|event| {
                    if let chat::Event::History { items, .. } = event {
                        Some(items)
                    } else {
                        None
                    }
                })
                .flatten()
                .collect();
            assert!(items.iter().any(|item| matches!(&item.content, chat::ItemContent::Text {text, ..} if text == "effective-B-body")));
            assert!(!items.iter().any(|item| matches!(&item.content, chat::ItemContent::Text {text, ..} if text == "issued-A-body")));
            // The real adapter's confirmed result samples its title reader.
            // The same prepared origin must supply both history and metadata.
            let result = adapter
                .receive(json!({"type":"result","session_id":CONVERSATION}))
                .unwrap();
            assert!(result.events.iter().any(|event| matches!(event,
                chat::Event::Session {id, title:Some(title)}
                    if id == CONVERSATION && title == "effective-B-body title")));
            assert!(!result.events.iter().any(|event| matches!(event,
                chat::Event::Session {title:Some(title), ..} if title == "issued-A-body title")));
            let mut bytes = [0_u8; 256];
            assert_eq!(
                watcher.read(&mut bytes).unwrap_err().kind(),
                std::io::ErrorKind::WouldBlock,
                "issued transcript must never be opened/indexed"
            );
            assert_eq!(launch, issued);
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn upgrade_history_validation_keeps_recorded_home_after_live_working_directory_disappears() {
        use std::{
            io::Read,
            os::fd::{AsRawFd, FromRawFd},
        };
        const ID: &str = "12345678-1234-4234-8234-123456789abc";
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().join("now-missing-project");
        let project: String = cwd
            .to_str()
            .unwrap()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        let home_a = root.path().join("recorded-home");
        let home_b = root.path().join("daemon-home");
        let mut watches = Vec::new();
        for (home, body) in [(&home_a, "recorded-A"), (&home_b, "unrelated-B")] {
            let file = home
                .join(".claude/projects")
                .join(&project)
                .join(format!("{ID}.jsonl"));
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(&file, json!({"type":"user","uuid":"private-prompt","parentUuid":null,"sessionId":ID,"message":{"role":"user","content":body}}).to_string()+"\n").unwrap();
            let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
            assert!(fd >= 0);
            let watcher = unsafe { std::fs::File::from_raw_fd(fd) };
            let path = std::ffi::CString::new(file.as_os_str().as_encoded_bytes()).unwrap();
            assert!(
                unsafe {
                    libc::inotify_add_watch(
                        watcher.as_raw_fd(),
                        path.as_ptr(),
                        libc::IN_OPEN | libc::IN_ACCESS,
                    )
                } >= 0
            );
            watches.push(watcher);
        }
        let launch = chat::Launch {
            provider: Provider::Claude,
            session_id: Some(ID.into()),
            recipe: LaunchRecipe {
                program: "private-provider-not-executed".into(),
                arguments: vec!["--resume".into(), ID.into()],
                working_directory: cwd,
                environment: [("HOME".into(), home_a.to_str().unwrap().into())].into(),
                unset_environment: Default::default(),
            },
        };
        std::fs::create_dir(&launch.recipe.working_directory).unwrap();
        assert!(!launch.needs_read_only_history());
        std::fs::remove_dir(&launch.recipe.working_directory).unwrap();
        assert!(launch.needs_read_only_history());
        let issued = launch.clone();
        let restored = upgrade_environment(
            &launch,
            false,
            [
                ("HOME".into(), home_b.to_str().unwrap().into()),
                (
                    "CLAUDE_CONFIG_DIR".into(),
                    home_b.join(".claude").to_str().unwrap().into(),
                ),
            ]
            .into(),
        )
        .unwrap();
        assert_eq!(restored, launch.recipe.environment);
        let mut bytes = [0_u8; 256];
        assert!(
            watches[0].read(&mut bytes).unwrap() > 0,
            "actual upgrade factory must read recorded A"
        );
        assert_eq!(
            watches[1].read(&mut bytes).unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock,
            "upgrade validation must not read daemon B"
        );
        assert_eq!(launch, issued);
    }

    #[test]
    fn removed_claude_account_is_reported_before_missing_resume_history() {
        const CHILD: &str = "CAGENTS_REMOVED_ACCOUNT_HISTORY_TEST";
        if std::env::var_os(CHILD).is_none() {
            let directory = tempfile::tempdir().unwrap();
            crate::secure_file::prepare_private_directory(&directory.path().join("runtime"))
                .unwrap();
            assert!(std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "host::chat::tests::removed_claude_account_is_reported_before_missing_resume_history", "--nocapture"])
                .env_clear().env(CHILD, "1").env("HOME", directory.path())
                .env("XDG_DATA_HOME", directory.path().join("data"))
                .env("XDG_CONFIG_HOME", directory.path().join("config"))
                .env("XDG_RUNTIME_DIR", directory.path().join("runtime"))
                .env("PATH", "/usr/bin:/bin")
                .status().unwrap().success());
            return;
        }
        let homes = crate::account::home::Homes::installation().unwrap();
        let store = crate::account::registry::Store::installation().unwrap();
        let (key, home) = store
            .create_pending(&homes, Provider::Claude, 1, || {
                homes.create(Provider::Claude, None)
            })
            .unwrap();
        let session = Session {
            id: TerminalId::new(),
            native_continuity: Default::default(),
            owned_codex_listener: None,
            proof: None,
            terminal: None,
            launch: chat::Launch {
                provider: Provider::Claude,
                recipe: LaunchRecipe {
                    program: "/bin/false".into(),
                    arguments: Vec::new(),
                    working_directory: std::env::current_dir().unwrap(),
                    environment: BTreeMap::from([(
                        "CLAUDE_CONFIG_DIR".into(),
                        home.to_str().unwrap().into(),
                    )]),
                    unset_environment: Default::default(),
                },
                session_id: Some("3a888c9d-7e33-47ad-9c0f-d538f63b9099".into()),
            },
        };
        store
            .remove_home(&homes, Provider::Claude, key, false)
            .unwrap();
        assert!(!home.exists());
        // Without the account preflight this exact adapter preparation error
        // used to hide Recovery::Account and leave the UI without its chooser.
        assert!(
            chat::protocol::create(&session.launch)
                .err()
                .unwrap()
                .contains("history project directory is unavailable")
        );
        let host = Arc::new(crate::host::state::HostLock::new(HostState::new(
            HostSessionProof {
                host_session_id: HostSessionId::new("removed-history"),
                boot_id: BootId::new("boot"),
                host_incarnation: HostIncarnation::new("incarnation"),
            },
        )));
        let failure = start(&host, 1, session, None, false, None, Default::default()).unwrap_err();
        assert_eq!(failure.code, ProtocolErrorCode::AccountRemoved);
        assert!(
            !home.exists(),
            "preparation never recreates the removed home"
        );
        assert!(host.lock().unwrap().chat_spawning.is_empty());
    }

    /// A conversation opened on a stopped agent terminal starts with the
    /// daemon's own provider configuration, so an upgrade restores it the same
    /// way rather than refusing it for a variable only the daemon supplies.
    #[test]
    fn an_upgrade_restores_the_environment_a_chat_actually_started_with() {
        let launch = chat::Launch {
            provider: Provider::Omp,
            recipe: LaunchRecipe {
                unset_environment: Default::default(),
                program: "omp".into(),
                arguments: vec!["--model".into(), "model".into()],
                working_directory: "/project".into(),
                environment: BTreeMap::from([
                    ("HOME".into(), "/home/user".into()),
                    ("PATH".into(), "/recipe/bin".into()),
                ]),
            },
            session_id: Some("01a0ab66-21d6-7065-9fe5-95f302b669e1".into()),
        };
        let daemon = BTreeMap::from([
            ("BUN_INSTALL_GLOBAL_STORE".into(), "/store".into()),
            ("PATH".into(), "/daemon/bin".into()),
        ]);
        let started = sourceless_environment(
            daemon.clone(),
            BTreeMap::new(),
            chat::protocol::create(&launch).unwrap().0.environment,
        );
        assert_eq!(started["BUN_INSTALL_GLOBAL_STORE"], "/store");
        assert_eq!(started["PATH"], "/recipe/bin");
        assert_eq!(
            upgrade_environment(&launch, false, daemon.clone()).unwrap(),
            started
        );
        // A conversation whose terminal is still hosted restarts from that
        // terminal, not from the daemon.
        assert_eq!(
            upgrade_environment(&launch, true, daemon).unwrap(),
            launch.recipe.environment
        );
    }

    #[test]
    fn selected_account_removals_survive_daemon_environment_and_upgrade() {
        let account = crate::account::Account::managed(
            Provider::Claude,
            uuid::Uuid::new_v4(),
            "/selected".into(),
        );
        let launch = chat::Launch {
            provider: Provider::Claude,
            recipe: LaunchRecipe {
                program: "claude".into(),
                arguments: Vec::new(),
                working_directory: "/project".into(),
                environment: BTreeMap::from([
                    ("CLAUDE_CONFIG_DIR".into(), "/selected".into()),
                    ("ANTHROPIC_API_KEY".into(), "recipe-fixture".into()),
                ]),
                unset_environment: account
                    .credential_overrides()
                    .iter()
                    .map(|name| (*name).into())
                    .collect(),
            },
            session_id: None,
        };
        let daemon = BTreeMap::from([
            ("ANTHROPIC_API_KEY".into(), "daemon-fixture".into()),
            ("HTTPS_PROXY".into(), "http://fixture".into()),
        ]);
        let selected = BTreeMap::from([("CLAUDE_CODE_OAUTH_TOKEN".into(), "gui-fixture".into())]);
        let (mut spec, _) = chat::protocol::create(&launch).unwrap();
        spec.environment = sourceless_environment(daemon.clone(), selected, spec.environment);
        spec.remove_unset_environment();
        assert_eq!(
            spec.environment
                .get("CLAUDE_CONFIG_DIR")
                .map(String::as_str),
            Some("/selected")
        );
        assert!(!spec.environment.contains_key("ANTHROPIC_API_KEY"));
        assert!(!spec.environment.contains_key("CLAUDE_CODE_OAUTH_TOKEN"));
        assert_eq!(
            spec.environment.get("HTTPS_PROXY").map(String::as_str),
            Some("http://fixture")
        );
        assert_eq!(
            upgrade_environment(&launch, false, daemon).unwrap(),
            spec.environment
        );
        assert!(
            !upgrade_environment(&launch, true, BTreeMap::new())
                .unwrap()
                .contains_key("ANTHROPIC_API_KEY")
        );
    }

    #[test]
    fn paired_codex_upgrade_preserves_confirmed_fresh_or_switched_thread() {
        use crate::model::agent_recovery::{AgentProcess, bind_session};
        const SELECTED: &str = "12345678-1234-4234-8234-123456789abc";
        const ORIGINAL: &str = "98765432-1234-4234-8234-123456789abc";
        let recipe = LaunchRecipe {
            unset_environment: Default::default(),
            program: "codex".into(),
            arguments: vec!["--profile".into(), "work".into()],
            working_directory: "/project".into(),
            environment: BTreeMap::from([("CODEX_HOME".into(), "/account".into())]),
        };
        let expected = bind_session(&recipe, SELECTED).unwrap();
        let launch = chat::Launch {
            provider: Provider::Codex,
            recipe: expected.clone(),
            session_id: Some(SELECTED.into()),
        };
        for source in [recipe.clone(), bind_session(&recipe, ORIGINAL).unwrap()] {
            let process = AgentProcess {
                pid: 42,
                start: 100,
                provider: Provider::Codex,
                launch: source.clone(),
                recovery: source,
            };
            assert_eq!(
                paired_upgrade_recipe(&process, &process, &launch, Some(SELECTED)).unwrap(),
                expected,
            );
            assert!(paired_upgrade_recipe(&process, &process, &launch, Some(ORIGINAL)).is_err());
            assert!(paired_upgrade_recipe(&process, &process, &launch, None).is_err());
            let mut replacement = process.clone();
            replacement.start += 1;
            assert!(
                paired_upgrade_recipe(&process, &replacement, &launch, Some(SELECTED)).is_err()
            );
            let mut other_account = process.clone();
            other_account
                .recovery
                .environment
                .insert("CODEX_HOME".into(), "/other".into());
            assert!(
                paired_upgrade_recipe(&process, &other_account, &launch, Some(SELECTED)).is_err()
            );
            // Matching process evidence alone cannot bless a stale native launch.
            assert!(
                paired_upgrade_recipe(&other_account, &other_account, &launch, Some(SELECTED))
                    .is_err()
            );
            for mutation in 0..5 {
                let mut changed = process.clone();
                match mutation {
                    0 => changed.recovery.program = "/other/codex".into(),
                    1 => changed.recovery.working_directory = "/other".into(),
                    2 => changed
                        .recovery
                        .arguments
                        .extend(["--model".into(), "other".into()]),
                    3 => {
                        changed
                            .recovery
                            .unset_environment
                            .insert("CODEX_API_KEY".into());
                    }
                    _ => changed
                        .recovery
                        .arguments
                        .extend(["--remote".into(), "unix:///other".into()]),
                }
                assert!(
                    paired_upgrade_recipe(&changed, &changed, &launch, Some(SELECTED)).is_err()
                );
            }
        }
    }

    #[test]
    fn paired_upgrade_preserves_issued_metadata_and_refuses_owner_changes() {
        let proof = |name| HostSessionProof {
            host_session_id: HostSessionId::new(name),
            boot_id: BootId::new("boot"),
            host_incarnation: HostIncarnation::new(name),
        };
        let continuity = |proof: HostSessionProof| crate::model::TerminalContinuity::Live {
            host_session_id: proof.host_session_id,
            boot_id: proof.boot_id,
            host_incarnation: proof.host_incarnation,
        };
        let mut issued = crate::terminal_ipc::TerminalDescriptor::relaunchable(
            TerminalId::new(),
            LaunchRecipe::shell("/project".into()).unwrap(),
            None,
        )
        .session;
        issued.continuity = continuity(proof("old"));
        issued.resume_hint = Some("original issued observation".into());
        let mut current = issued.clone();
        current.continuity = continuity(proof("new"));
        current.resume_hint = Some("current worker observation".into());
        let mut restored = restored_terminal_source(&issued, &current).unwrap();
        assert_eq!(restored.continuity, current.continuity);
        restored.continuity = issued.continuity.clone();
        assert_eq!(restored, issued);
        for mutation in 0..9 {
            let mut changed = current.clone();
            match mutation {
                0 => changed.id = TerminalId::new(),
                1 => changed.launch.program = "/another".into(),
                2 => changed.launch.arguments.push("another".into()),
                3 => changed.launch.working_directory = "/another".into(),
                4 => {
                    changed
                        .launch
                        .environment
                        .insert("CODEX_HOME".into(), "/another".into());
                }
                5 => {
                    changed
                        .launch
                        .unset_environment
                        .insert("CODEX_API_KEY".into());
                }
                6 => changed.transcript_path = Some("/another".into()),
                7 => changed.recovery_origin = Some(TerminalId::new()),
                _ => changed.predecessor = Some(TerminalId::new()),
            }
            assert!(restored_terminal_source(&issued, &changed).is_err());
        }
        current.continuity = crate::model::TerminalContinuity::RelaunchAvailable {
            recipe: current.launch.clone(),
            last_exit: None,
        };
        assert!(restored_terminal_source(&issued, &current).is_err());
    }

    #[test]
    fn confirmed_selection_is_bound_to_process_account_identity_and_expiration() {
        let now = Instant::now();
        let id = "12345678-1234-4234-8234-123456789abc";
        let recipe = LaunchRecipe {
            unset_environment: Default::default(),
            program: "codex".into(),
            arguments: Vec::new(),
            working_directory: "/same".into(),
            environment: BTreeMap::from([("HOME".into(), "/account".into())]),
        };
        let process = crate::model::agent_recovery::AgentProcess {
            pid: 42,
            start: 100,
            provider: Provider::Codex,
            launch: recipe.clone(),
            recovery: recipe,
        };
        let binding = PendingBinding {
            preview: chat::BindingPreview {
                token: "single-use".into(),
                session_id: id.into(),
                title: "Chosen".into(),
                working_directory: "/same".into(),
            },
            process: process.clone(),
            expires: now + Duration::from_secs(1),
        };
        assert!(binding.authorizes(&process, id, now));
        assert!(!binding.authorizes(&process, "98765432-1234-4234-8234-123456789abc", now));
        assert!(!binding.authorizes(&process, id, binding.expires));
        let mut replacement = process.clone();
        replacement.start += 1;
        assert!(!binding.authorizes(&replacement, id, now));
        let mut other_account = process;
        other_account
            .recovery
            .environment
            .insert("HOME".into(), "/other-account".into());
        assert!(!binding.authorizes(&other_account, id, now));
    }

    #[test]
    fn a_candidate_listing_names_a_terminal_where_a_preview_names_a_conversation() {
        let (_directory, host, _saved) = upgrade_fixture();
        let source = host
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .proof()
            .clone();
        let session = Session {
            native_continuity: Default::default(),
            owned_codex_listener: None,
            id: TerminalId::new(),
            launch: chat::Launch {
                provider: Provider::Codex,
                recipe: LaunchRecipe {
                    unset_environment: Default::default(),
                    program: "codex".into(),
                    arguments: Vec::new(),
                    working_directory: "/project".into(),
                    environment: BTreeMap::new(),
                },
                session_id: None,
            },
            proof: Some(source.clone()),
            terminal: None,
        };
        // A listing names a terminal, never a conversation, so the missing ID
        // that a preview rejects outright still reaches the terminal lookup.
        let listed = process(
            &host,
            1,
            ClientRequest::ChatCandidates {
                session: session.clone(),
                source: source.clone(),
            },
        )
        .unwrap_err();
        assert_eq!(listed.code, ProtocolErrorCode::TerminalUnavailable);
        let previewed = process(
            &host,
            1,
            ClientRequest::ChatPreview {
                session: session.clone(),
                source: source.clone(),
            },
        )
        .unwrap_err();
        assert_eq!(previewed.code, ProtocolErrorCode::InvalidRequest);
        // An activation still has to consume a token issued by a real preview,
        // and no listing can produce one.
        let activated = process(
            &host,
            1,
            ClientRequest::ChatStartSelected {
                session,
                source,
                token: "forged".into(),
            },
        )
        .unwrap_err();
        assert_eq!(activated.code, ProtocolErrorCode::InvalidRequest);
    }

    fn upgrade_fixture() -> (tempfile::TempDir, SharedHost, crate::upgrade::handoff::Chat) {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("omp");
        std::fs::write(&executable, r#"#!/usr/bin/env python3
import json, os, sys
identity = sys.argv[sys.argv.index("--resume") + 1]
print(json.dumps({"type": "ready"}), flush=True)
for line in sys.stdin:
    request = json.loads(line)
    command = request["type"]
    if command not in ("negotiate_protocol", "get_state", "get_available_models", "get_available_commands", "get_session_stats", "get_messages_page", "get_subagents", "set_subagent_subscription"):
        sys.exit(7)
    data = {"sessionId": os.environ.get("REPORTED_SESSION", identity), "messages": []}
    print(json.dumps({"type": "response", "id": request["id"], "command": command, "success": True, "data": data}), flush=True)
"#).unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let old = HostSessionProof {
            host_session_id: HostSessionId::new("old-session"),
            boot_id: BootId::new("boot"),
            host_incarnation: HostIncarnation::new("old-incarnation"),
        };
        let current = HostSessionProof {
            host_session_id: HostSessionId::new("new-session"),
            boot_id: BootId::new("boot"),
            host_incarnation: HostIncarnation::new("new-incarnation"),
        };
        let launch = chat::Launch {
            provider: Provider::Omp,
            recipe: LaunchRecipe {
                unset_environment: Default::default(),
                program: executable.to_string_lossy().into_owned(),
                arguments: Vec::new(),
                working_directory: directory.path().to_owned(),
                environment: BTreeMap::new(),
            },
            session_id: Some("12345678-1234-4234-8234-123456789abc".into()),
        };
        let source = Session {
            native_continuity: Default::default(),
            owned_codex_listener: None,
            id: TerminalId::new(),
            launch: launch.clone(),
            proof: Some(old),
            terminal: None,
        };
        (
            directory,
            Arc::new(crate::host::state::HostLock::new(HostState::new(current))),
            crate::upgrade::handoff::Chat {
                source,
                launch,
                previous_sources: Vec::new(),
            },
        )
    }

    #[test]
    fn missing_claude_directory_restores_a_hosted_history_reader_without_authority() {
        let (root, host, mut saved) = upgrade_fixture();
        let cwd = root.path().join("deleted-project");
        let config = root.path().join("claude-config");
        let project: String = cwd
            .to_str()
            .unwrap()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        let transcript = config
            .join("projects")
            .join(project)
            .join("12345678-1234-4234-8234-123456789abc.jsonl");
        std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        std::fs::write(&transcript, serde_json::json!({"type":"user","uuid":"saved-prompt","parentUuid":null,"sessionId":"12345678-1234-4234-8234-123456789abc","message":{"role":"user","content":"A prompt saved before the worktree was removed"}}).to_string() + "\n").unwrap();
        saved.launch.provider = Provider::Claude;
        saved.launch.recipe.program = "claude".into();
        saved.launch.recipe.working_directory = cwd;
        saved.launch.recipe.environment = BTreeMap::from([(
            "CLAUDE_CONFIG_DIR".into(),
            config.to_string_lossy().into_owned(),
        )]);
        saved.source.launch = saved.launch.clone();
        saved.source.proof = None;
        let response = start(
            &host,
            1,
            saved.source.clone(),
            None,
            false,
            None,
            Default::default(),
        )
        .unwrap();
        assert!(matches!(response, ServerResponse::ChatAttached { .. }));
        let handle = host.lock().unwrap_or_else(|p| p.into_inner()).chats[&saved.source.id]
            .handle
            .clone();
        handle.set_visible(true);
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if handle.snapshot(None).unwrap().exited {
                break;
            }
            assert!(Instant::now() < deadline);
            thread::yield_now();
        }
        let snapshot = handle.snapshot(None).unwrap();
        assert_eq!(snapshot.session_id, saved.launch.session_id);
        assert_eq!(snapshot.items.len(), 1);
        assert!(!handle.owns_work());
        assert!(
            handle.confirmed_session_id().is_none(),
            "disk identity is not provider confirmation"
        );
        assert!(
            !handle
                .execution_environment()
                .0
                .contains_key(crate::agent_execution::AUTHORITY_ENV)
        );
        assert!(
            handle
                .command(chat::Command::Prompt {
                    text: "do not run".into(),
                    attachments: Vec::new(),
                    mode: chat::PromptMode::Send
                })
                .unwrap_err()
                .contains("read-only")
        );
        handle.command(chat::Command::Refresh).unwrap();
        let failure = restore_upgrade(&host, &saved).unwrap_err();
        assert!(failure.contains("No provider was restarted"));
        assert!(
            host.lock()
                .unwrap_or_else(|p| p.into_inner())
                .upgrade_chats
                .is_empty()
        );
        handle.close().unwrap();
    }

    #[test]
    fn distinct_recovery_candidates_share_one_conversation_spawn_claim() {
        let (_directory, host, saved) = upgrade_fixture();
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let mut candidates = Vec::new();
        for client in [1, 2] {
            let mut candidate = saved.source.clone();
            candidate.id = TerminalId::new();
            candidate.proof = None;
            let host = Arc::clone(&host);
            let barrier = Arc::clone(&barrier);
            candidates.push(thread::spawn(move || {
                barrier.wait();
                (
                    client,
                    start(
                        &host,
                        client,
                        candidate,
                        None,
                        false,
                        None,
                        Default::default(),
                    ),
                )
            }));
        }
        barrier.wait();
        let outcomes = candidates
            .into_iter()
            .map(|task| task.join().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            outcomes.iter().filter(|(_, result)| result.is_ok()).count(),
            1
        );
        let (client, id) = outcomes
            .into_iter()
            .find_map(|(client, result)| match result {
                Ok(ServerResponse::ChatAttached { session, .. }) => Some((client, session.id)),
                _ => None,
            })
            .unwrap();
        let mut retry = saved.source;
        retry.id = TerminalId::new();
        retry.proof = None;
        assert!(start(&host, 3, retry, None, false, None, Default::default()).is_err());
        assert_eq!(host.lock().unwrap().chats.len(), 1);
        process(&host, client, ClientRequest::ChatClose { chat_id: id }).unwrap();
    }

    #[test]
    fn durable_terminal_commit_retires_evidence_after_terminal_retirement() {
        let (_directory, host, saved) = upgrade_fixture();
        let source_id = saved.source.id;
        let recipe = saved.launch.terminal_recipe().unwrap();
        let terminal_id = TerminalId::new();
        let terminal = crate::model::TerminalSession {
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
            transcript_path: None,
        };
        host.lock()
            .unwrap_or_else(|p| p.into_inner())
            .chat_handoffs
            .insert(
                source_id,
                TerminalHandoff::new(
                    saved.source,
                    terminal,
                    1,
                    BTreeMap::new(),
                    Instant::now() + ACKNOWLEDGEMENT_GRACE,
                ),
            );
        // No terminal record remains, but the proof-bound terminal ID still
        // names the durable replacement. EOF/terminal close did not consume it.
        for _ in 0..2 {
            assert!(matches!(
                process(&host, 1, ClientRequest::ChatCommitTerminal { terminal_id }).unwrap(),
                ServerResponse::Accepted
            ));
        }
        assert!(
            !host
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .chat_handoffs
                .contains_key(&source_id)
        );
    }

    #[test]
    fn standalone_credentials_survive_failed_and_retried_terminal_handoff_without_persistence() {
        let (directory, host, saved) = upgrade_fixture();
        std::fs::write(
            directory.path().join("omp"),
            r#"#!/usr/bin/env python3
import json, os, pathlib, sys, time
assert os.environ["ANTHROPIC_API_KEY"] == "gui-only-key"
assert os.environ["HTTPS_PROXY"] == "http://user:password@proxy.invalid:8080"
assert os.environ["PI_CACHE_RETENTION"] == "long"
assert os.environ["GIT_CONFIG_KEY_0"] == "http.extraHeader"
assert os.environ["GIT_CONFIG_VALUE_0"] == "Authorization: Bearer git-private-token"
assert "PI_TOOL_BRIDGE_TOKEN" not in os.environ
identity = "12345678-1234-4234-8234-123456789abc"
if "--mode" not in sys.argv:
    assert sys.argv[sys.argv.index("--resume") + 1] == identity
    with pathlib.Path("terminal-starts").open("a") as starts:
        starts.write("ok\n")
    for line in sys.stdin:
        pass
    sys.exit(0)
pathlib.Path("environment-received").write_text("ok")
print(json.dumps({"type": "ready"}), flush=True)
for line in sys.stdin:
    while not pathlib.Path("publish-identity").exists():
        time.sleep(0.01)
    request = json.loads(line)
    data = {"sessionId": identity, "messages": []}
    print(json.dumps({"type": "response", "id": request["id"], "command": request["type"], "success": True, "data": data}), flush=True)
"#,
        )
        .unwrap();
        let mut session = saved.source;
        session.proof = None;
        session.launch.session_id = None;
        let execution_environment =
            crate::terminal_ipc::ChatExecutionEnvironment(BTreeMap::from([
                ("ANTHROPIC_API_KEY".into(), "gui-only-key".into()),
                (
                    "HTTPS_PROXY".into(),
                    "http://user:password@proxy.invalid:8080".into(),
                ),
                ("PI_CACHE_RETENTION".into(), "long".into()),
                ("GIT_CONFIG_COUNT".into(), "1".into()),
                ("GIT_CONFIG_KEY_0".into(), "http.extraHeader".into()),
                (
                    "GIT_CONFIG_VALUE_0".into(),
                    "Authorization: Bearer git-private-token".into(),
                ),
            ]));
        let request = ClientRequest::ChatStart {
            session: session.clone(),
            source: None,
            terminate_source: false,
            execution_environment,
        };
        assert!(!format!("{request:?}").contains("gui-only-key"));
        assert!(!format!("{request:?}").contains("user:password"));
        let response = process(&host, 1, request).unwrap();
        let ServerResponse::ChatAttached {
            session: attached, ..
        } = response
        else {
            panic!("standalone chat did not attach");
        };
        assert!(
            !attached
                .launch
                .recipe
                .environment
                .contains_key("ANTHROPIC_API_KEY")
        );
        assert!(
            !attached
                .launch
                .recipe
                .environment
                .contains_key("HTTPS_PROXY")
        );
        let handle = host.lock().unwrap_or_else(|p| p.into_inner()).chats[&session.id]
            .handle
            .clone();
        let resources_alive = handle.resource_lifetime();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !directory.path().join("environment-received").exists() {
            assert!(
                Instant::now() < deadline,
                "provider did not receive GUI execution environment"
            );
            thread::sleep(Duration::from_millis(10));
        }
        // A successful process spawn is not a durable fresh conversion boundary.
        let mut launch = attached.launch.clone();
        assert!(confirm_fresh_session(&handle, &mut launch, Instant::now(), || false).is_err());
        assert_eq!(launch.session_id, None);
        std::fs::write(directory.path().join("publish-identity"), "").unwrap();
        confirm_fresh_session(
            &handle,
            &mut launch,
            Instant::now() + Duration::from_secs(5),
            || false,
        )
        .unwrap();
        assert_eq!(
            launch.session_id.as_deref(),
            Some("12345678-1234-4234-8234-123456789abc")
        );
        assert!(launch.terminal_recipe().is_some());
        let capability = host
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .shutdown_preflight(Instant::now())
            .unwrap()
            .capability;
        let failure = super::super::state::prepare_upgrade(&host, capability, false).unwrap_err();
        assert!(failure.message.contains("execution environment"));
        assert!(!handle.is_completed());
        assert!(!host.lock().unwrap_or_else(|p| p.into_inner()).shutting_down);

        // Losing the executable after native startup must retain the selected
        // account for retry, even though the native actor has already closed.
        std::fs::rename(
            directory.path().join("omp"),
            directory.path().join("held-omp"),
        )
        .unwrap();
        assert!(
            process(
                &host,
                1,
                ClientRequest::ChatReturnTerminal {
                    chat_id: session.id
                }
            )
            .is_err()
        );
        assert!(handle.is_completed());
        assert!(
            host.lock()
                .unwrap_or_else(|p| p.into_inner())
                .chats
                .contains_key(&session.id)
        );
        assert!(
            !host
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .chat_handoffs
                .contains_key(&session.id)
        );
        let capability = host
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .shutdown_preflight(Instant::now())
            .unwrap()
            .capability;
        let failure = super::super::state::prepare_upgrade(&host, capability, false).unwrap_err();
        assert!(failure.message.contains("replacement is still pending"));
        std::fs::rename(
            directory.path().join("held-omp"),
            directory.path().join("omp"),
        )
        .unwrap();
        let ServerResponse::ChatTerminal {
            session: terminal, ..
        } = process(
            &host,
            1,
            ClientRequest::ChatReturnTerminal {
                chat_id: session.id,
            },
        )
        .unwrap()
        else {
            panic!("native chat did not return its terminal");
        };
        assert!(terminal.continuity.live_proof().is_some());
        drop(handle);
        assert!(
            !resources_alive(),
            "successful handoff retained closed actor resources"
        );
        {
            let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
            assert!(!state.chats.contains_key(&session.id));
            state.unregister_client(1);
            state.register_client(2);
        }
        assert!(
            process(
                &host,
                2,
                ClientRequest::ChatReturnTerminal {
                    chat_id: session.id
                }
            )
            .is_err()
        );
        let ServerResponse::ChatAttached { snapshot, .. } = process(
            &host,
            2,
            ClientRequest::ChatAttach {
                session: attached.clone(),
            },
        )
        .unwrap() else {
            panic!("lost response did not permit exact reattachment");
        };
        assert!(snapshot.exited);
        assert_eq!(snapshot.session_id, launch.session_id);
        assert!(
            process(
                &host,
                2,
                ClientRequest::ChatClose {
                    chat_id: session.id
                }
            )
            .is_err()
        );
        assert!(
            process(
                &host,
                2,
                ClientRequest::ChatStart {
                    session: attached.clone(),
                    source: None,
                    terminate_source: false,
                    execution_environment: Default::default(),
                }
            )
            .is_err()
        );
        process(
            &host,
            2,
            ClientRequest::ChatVisibility {
                chat_id: session.id,
                visible: true,
            },
        )
        .unwrap();
        let ServerResponse::ChatSnapshot {
            snapshot: Some(polled),
            ..
        } = process(
            &host,
            2,
            ClientRequest::ChatRead {
                chat_id: session.id,
                after_generation: None,
            },
        )
        .unwrap()
        else {
            panic!("reattached receipt did not provide closed metadata");
        };
        assert!(polled.exited);
        let ServerResponse::ChatListed { sessions } =
            process(&host, 2, ClientRequest::ChatList).unwrap()
        else {
            panic!("inventory failed");
        };
        assert!(sessions.is_empty());
        let ServerResponse::ChatTerminal {
            session: repeated, ..
        } = process(
            &host,
            2,
            ClientRequest::ChatReturnTerminal {
                chat_id: session.id,
            },
        )
        .unwrap()
        else {
            panic!("lost handoff response was not replayed");
        };
        assert_eq!(repeated, terminal);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !directory.path().join("terminal-starts").exists() {
            assert!(
                Instant::now() < deadline,
                "CLI did not receive the native execution environment"
            );
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            std::fs::read_to_string(directory.path().join("terminal-starts")).unwrap(),
            "ok\n"
        );
        for durable in [
            serde_json::to_string(&attached).unwrap(),
            serde_json::to_string(&terminal).unwrap(),
        ] {
            for secret in ["gui-only-key", "user:password", "git-private-token"] {
                assert!(!durable.contains(secret));
            }
        }
        let terminal_handle = host.lock().unwrap_or_else(|p| p.into_inner()).terminals
            [&terminal.id]
            .handle
            .clone();
        let capability = host
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .shutdown_preflight(Instant::now())
            .unwrap()
            .capability;
        let failure = super::super::state::prepare_upgrade(&host, capability, false).unwrap_err();
        assert!(failure.message.contains("not durably acknowledged"));
        process(
            &host,
            2,
            ClientRequest::ChatCommitTerminal {
                terminal_id: TerminalId::new(),
            },
        )
        .unwrap();
        assert!(
            host.lock()
                .unwrap_or_else(|p| p.into_inner())
                .chat_handoffs
                .contains_key(&session.id)
        );
        // Receipt remains replayable until the caller has durably saved this
        // exact replacement. Losing the commit response permits the same ACK.
        for _ in 0..2 {
            assert!(matches!(
                process(
                    &host,
                    2,
                    ClientRequest::ChatCommitTerminal {
                        terminal_id: terminal.id,
                    }
                )
                .unwrap(),
                ServerResponse::Accepted
            ));
        }
        assert!(
            !host
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .chat_handoffs
                .contains_key(&session.id)
        );
        assert!(
            process(
                &host,
                2,
                ClientRequest::ChatAttach {
                    session: attached.clone()
                }
            )
            .is_err()
        );
        let capability = host
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .shutdown_preflight(Instant::now())
            .unwrap()
            .capability;
        let failure = super::super::state::prepare_upgrade(&host, capability, false).unwrap_err();
        assert!(failure.message.contains("execution environment"));
        assert!(!terminal_handle.is_completed());
        assert!(!host.lock().unwrap_or_else(|p| p.into_inner()).shutting_down);
        terminal_handle.shutdown().unwrap();
    }

    #[test]
    fn native_upgrade_restores_exact_provider_session_and_only_resolves_proven_source() {
        let (_directory, host, mut saved) = upgrade_fixture();
        saved.launch.recipe = saved.launch.terminal_recipe().unwrap();
        assert_ne!(saved.launch.recipe, saved.source.launch.recipe);
        restore_upgrade(&host, &saved).unwrap();
        let restored = resolve_upgrade(&host, &saved.source).unwrap();
        assert_eq!(restored.id, saved.source.id);
        assert_eq!(restored.launch.session_id, saved.launch.session_id);
        assert_eq!(restored.launch.recipe, saved.source.launch.recipe);
        assert_eq!(
            restored.proof.as_ref(),
            Some(host.lock().unwrap_or_else(|p| p.into_inner()).proof())
        );
        assert_ne!(restored.proof, saved.source.proof);
        let mut unissued = saved.source.clone();
        unissued.launch.session_id = Some("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa".into());
        assert!(
            resolve_upgrade(&host, &unissued).is_err(),
            "a matching old proof cannot substitute an unissued conversation"
        );
        assert!(saved.source.launch.session_id.is_some());
        unissued.launch.session_id = None;
        assert!(
            resolve_upgrade(&host, &unissued).is_err(),
            "known identity cannot be erased into manufactured pre-ID authority"
        );
        let mut wrong_source = saved.source.clone();
        wrong_source.proof = restored.proof;
        assert!(resolve_upgrade(&host, &wrong_source).is_err());
        let mut unrelated_thread = saved.source.clone();
        unrelated_thread.launch.session_id = Some("98765432-1234-4234-8234-123456789abc".into());
        assert!(resolve_upgrade(&host, &unrelated_thread).is_err());
        assert!(restore_upgrade(&host, &saved).is_err());
        let handle = host.lock().unwrap_or_else(|p| p.into_inner()).chats[&saved.source.id]
            .handle
            .clone();
        handle.close().unwrap();
    }

    #[test]
    fn native_upgrade_rejects_provider_substituting_another_conversation() {
        let (_directory, host, mut saved) = upgrade_fixture();
        saved.launch.recipe.environment.insert(
            "REPORTED_SESSION".into(),
            "98765432-1234-4234-8234-123456789abc".into(),
        );
        saved.source.launch.recipe.environment = saved.launch.recipe.environment.clone();
        assert!(restore_upgrade(&host, &saved).is_err());
        assert!(resolve_upgrade(&host, &saved.source).is_err());
        let handle = host.lock().unwrap_or_else(|p| p.into_inner()).chats[&saved.source.id]
            .handle
            .clone();
        handle
            .close()
            .expect("failed provider teardown permits explicit retirement");
        assert!(
            handle.failure().is_some(),
            "the rejected session remains diagnosed"
        );
    }

    #[test]
    fn terminal_close_retires_only_the_exact_closed_actors_recovered_pane_aliases() {
        let (directory, host, saved) = upgrade_fixture();
        restore_upgrade(&host, &saved).unwrap();
        let replacement = resolve_upgrade(&host, &saved.source).unwrap();
        let evidence = Arc::new(crate::upgrade::handoff::Store::open(directory.path()).unwrap());
        evidence
            .record_recovered(&[crate::upgrade::handoff::RecoveredChat {
                evidence: saved.clone(),
                replacement: replacement.clone(),
                aliases: Vec::new(),
            }])
            .unwrap();
        let handle = {
            let mut state = host.lock().unwrap();
            state.upgrade_evidence = Some(evidence.clone());
            state.chats[&saved.source.id].handle.clone()
        };
        assert!(
            evidence
                .recovered_sources(&saved.source, &replacement)
                .unwrap()
                .is_some()
        );
        close_terminal_actor(&host, saved.source.id, &handle).unwrap();
        assert!(!host.lock().unwrap().chats.contains_key(&saved.source.id));
        assert!(
            evidence
                .recovered_sources(&saved.source, &replacement)
                .unwrap()
                .is_none()
        );
        // An old close receipt must not clear evidence for another live actor.
        restore_upgrade(&host, &saved).unwrap();
        let replacement = resolve_upgrade(&host, &saved.source).unwrap();
        evidence
            .record_recovered(&[crate::upgrade::handoff::RecoveredChat {
                evidence: saved.clone(),
                replacement: replacement.clone(),
                aliases: Vec::new(),
            }])
            .unwrap();
        close_terminal_actor(&host, saved.source.id, &handle).unwrap();
        assert!(
            evidence
                .recovered_sources(&saved.source, &replacement)
                .unwrap()
                .is_some()
        );
        let current = host.lock().unwrap().chats[&saved.source.id].handle.clone();
        close_terminal_actor(&host, saved.source.id, &current).unwrap();
    }

    #[test]
    fn upgrade_resolution_checks_durable_older_aliases_when_ram_has_only_newer_sources() {
        let (directory, host, mut saved) = upgrade_fixture();
        saved.launch.recipe = saved.launch.terminal_recipe().unwrap();
        restore_upgrade(&host, &saved).unwrap();
        let restored = resolve_upgrade(&host, &saved.source).unwrap();
        let evidence = Arc::new(crate::upgrade::handoff::Store::open(directory.path()).unwrap());
        evidence
            .record_recovered(&[crate::upgrade::handoff::RecoveredChat {
                evidence: saved.clone(),
                replacement: restored.clone(),
                aliases: Vec::new(),
            }])
            .unwrap();
        let mut newer = saved.clone();
        newer.source.proof.as_mut().unwrap().host_incarnation =
            HostIncarnation::new("intermediate-daemon");
        {
            let mut state = host.lock().unwrap();
            state.upgrade_evidence = Some(evidence);
            state.upgrade_chats.insert(
                saved.source.id,
                vec![crate::upgrade::handoff::ChatSource {
                    source: newer.source,
                    launch: newer.launch,
                }],
            );
        }
        assert_eq!(resolve_upgrade(&host, &saved.source).unwrap(), restored);
        for mutation in 0..4 {
            let mut changed = saved.source.clone();
            match mutation {
                0 => {
                    changed.proof.as_mut().unwrap().host_incarnation =
                        HostIncarnation::new("unrelated-daemon")
                }
                1 => {
                    changed
                        .launch
                        .recipe
                        .environment
                        .insert("HOME".into(), "/another-account".into());
                }
                2 => {
                    changed.launch.session_id = Some("98765432-1234-4234-8234-123456789abc".into())
                }
                _ => changed
                    .launch
                    .recipe
                    .arguments
                    .push("--changed-listener".into()),
            }
            assert!(resolve_upgrade(&host, &changed).is_err());
        }
        let handle = host.lock().unwrap().chats[&saved.source.id].handle.clone();
        handle.close().unwrap();
    }

    #[test]
    fn legacy_codex_upgrade_publishes_issued_arguments_and_hydrates_the_exact_thread() {
        const CHILD: &str = "CAGENTS_LEGACY_CODEX_UPGRADE_CONTRACT";
        let exact = thread::current()
            .name()
            .expect("named owning contract")
            .to_owned();
        if std::env::var(CHILD).as_deref() != Ok(exact.as_str()) {
            use crate::repository::{
                CancellationToken, CommandSpec, ProcessExecutor, SystemProcessExecutor,
            };
            // Keep private evidence on success AND failure; a child result does
            // not establish that every descendant has positively retired.
            let root = tempfile::Builder::new()
                .prefix("cagents-legacy-upgrade-")
                .tempdir_in("/var/tmp")
                .unwrap()
                .keep();
            for name in ["home", "run", "data", "config", "cache"] {
                use std::os::unix::fs::PermissionsExt;
                std::fs::create_dir(root.join(name)).unwrap();
                std::fs::set_permissions(root.join(name), std::fs::Permissions::from_mode(0o700))
                    .unwrap();
            }
            eprintln!(
                "legacy upgrade owning evidence retained at {}",
                root.display()
            );
            let command = CommandSpec::new(std::env::current_exe().unwrap())
                .args(["--exact", exact.as_str(), "--nocapture", "--test-threads=1"])
                .env(CHILD, &exact)
                .env("HOME", root.join("home"))
                .env("XDG_RUNTIME_DIR", root.join("run"))
                .env("XDG_DATA_HOME", root.join("data"))
                .env("XDG_CONFIG_HOME", root.join("config"))
                .env("XDG_CACHE_HOME", root.join("cache"))
                .timeout(Duration::from_secs(35))
                .output_limits(128 * 1024, 128 * 1024);
            let output = SystemProcessExecutor
                .execute(&command, &CancellationToken::new())
                .unwrap();
            std::fs::write(root.join("stdout.log"), &output.stdout).unwrap();
            std::fs::write(root.join("stderr.log"), &output.stderr).unwrap();
            let output = output.require_success(&command.program).unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(
                stdout.lines().any(|line| line == "running 1 test")
                    && stdout
                        .lines()
                        .any(|line| line == format!("test {exact} ... ok"))
                    && stdout
                        .lines()
                        .any(|line| line.starts_with("test result: ok. 1 passed; 0 failed;")),
                "exact owning contract did not execute; retained evidence {}",
                root.display()
            );
            return;
        }
        use std::os::unix::net::UnixListener;
        let (directory, host, mut saved) = upgrade_fixture();
        let socket = directory.path().join("codex.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let home = directory.path().join("codex-home");
        std::fs::create_dir(&home).unwrap();
        let identity = saved.launch.session_id.clone().unwrap();
        let expected = identity.clone();
        let reported_home = home.clone();
        let (finished, completion) = std::sync::mpsc::channel();
        let server = thread::spawn(move || {
            listener.set_nonblocking(true).unwrap();
            let deadline = Instant::now() + Duration::from_secs(12);
            let stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            Instant::now() < deadline,
                            "exact Codex peer never connected"
                        );
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => panic!("fixture accept: {error}"),
                }
            };
            stream
                .set_read_timeout(Some(Duration::from_millis(200)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut socket = tungstenite::accept(stream).unwrap();
            loop {
                assert!(Instant::now() < deadline, "exact Codex peer did not close");
                let message = match socket.read() {
                    Ok(message) => message,
                    Err(tungstenite::Error::Io(error))
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) =>
                    {
                        continue;
                    }
                    Err(_) => break,
                };
                let tungstenite::Message::Text(text) = message else {
                    continue;
                };
                let frame: Value = serde_json::from_str(&text).unwrap();
                if frame.get("id").is_none() {
                    continue;
                }
                let result = match frame["method"].as_str().unwrap() {
                    "initialize" => {
                        serde_json::json!({"userAgent":"cagents/0.1.0", "codexHome":reported_home})
                    }
                    "thread/resume" => {
                        assert_eq!(frame["params"]["threadId"], expected);
                        serde_json::json!({"thread":{"id":expected,"canAcceptDirectInput":true,"status":{"type":"idle"}},
                            "model":"fixture-luna", "initialTurnsPage":{"data":[{"id":"saved-turn","status":"completed","items":[
                                {"type":"userMessage","id":"saved-prompt","content":[{"type":"text","text":"existing prompt"}]},
                                {"type":"agentMessage","id":"saved-answer","text":"existing exact-thread answer","phase":"final_answer"}
                            ]}],"nextCursor":null}})
                    }
                    "model/list" | "skills/list" | "thread/list" | "thread/queue/list" => {
                        serde_json::json!({"data":[],"nextCursor":null})
                    }
                    method => panic!("unexpected fixture method {method}"),
                };
                if socket
                    .send(tungstenite::Message::Text(
                        serde_json::json!({"id":frame["id"],"result":result})
                            .to_string()
                            .into(),
                    ))
                    .is_err()
                {
                    break;
                }
            }
            let _ = finished.send(());
        });
        saved.source.launch.provider = Provider::Codex;
        saved.source.launch.recipe.program = "codex".into();
        saved.source.launch.recipe.arguments =
            vec!["--remote".into(), format!("unix://{}", socket.display())];
        saved
            .source
            .launch
            .recipe
            .environment
            .insert("CODEX_HOME".into(), home.display().to_string());
        saved.launch = saved.source.launch.clone();
        saved.launch.recipe = saved.launch.terminal_recipe().unwrap();
        assert_ne!(saved.launch.recipe, saved.source.launch.recipe);
        restore_upgrade(&host, &saved).unwrap();
        let (replacement, handle) = {
            let state = host.lock().unwrap();
            let record = &state.chats[&saved.source.id];
            (record.session.clone(), record.handle.clone())
        };
        assert_eq!(replacement.launch.recipe, saved.source.launch.recipe);
        assert_eq!(replacement.launch.session_id, Some(identity));
        assert!(chat::is_upgrade_recovery(&saved.source, &replacement));
        let deadline = Instant::now() + Duration::from_secs(5);
        let snapshot = loop {
            let snapshot = handle.snapshot(None).unwrap();
            if snapshot.ready && !snapshot.history_loading {
                break snapshot;
            }
            assert!(
                Instant::now() < deadline,
                "exact-thread fixture did not become ready: {snapshot:?}"
            );
            thread::sleep(Duration::from_millis(10));
        };
        assert!(snapshot.ready);
        assert!(snapshot.items.iter().any(|item| matches!(&item.content,
            chat::ItemContent::Text { text, .. } if text == "existing exact-thread answer")));
        handle.close().unwrap();
        completion
            .recv_timeout(Duration::from_secs(3))
            .expect("bounded Codex fixture completion");
        server.join().unwrap();
    }

    #[test]
    fn native_restart_cannot_change_the_issued_account_environment() {
        let (_directory, host, mut saved) = upgrade_fixture();
        saved
            .launch
            .recipe
            .environment
            .insert("HOME".into(), "/another/account".into());
        assert_eq!(
            restore_upgrade(&host, &saved).unwrap_err(),
            "Upgrade native source recipe is inconsistent"
        );
        assert!(
            host.lock()
                .unwrap_or_else(|p| p.into_inner())
                .chats
                .is_empty()
        );
    }

    struct Echo(bool);
    #[test]
    fn automatic_handoff_fences_idle_state_and_confirms_the_same_conversation() {
        account_handoff_case(false, false);
    }

    #[test]
    fn explicit_pinned_handoff_keeps_original_directory_and_confirms_the_same_conversation() {
        account_handoff_case(true, false);
    }

    #[test]
    fn removed_account_target_refuses_before_closing_the_healthy_original() {
        const CHILD: &str = "CAGENTS_TEST_REMOVED_ACCOUNT_SWITCH_TARGET";
        if std::env::var_os(CHILD).is_none() {
            use std::io::Read;
            use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
            let private = tempfile::Builder::new()
                .prefix("switch-target-")
                .permissions(std::fs::Permissions::from_mode(0o700))
                .tempdir()
                .unwrap();
            for leaf in ["home", "runtime", "config", "data", "tmp"] {
                std::fs::create_dir(private.path().join(leaf)).unwrap();
                std::fs::set_permissions(
                    private.path().join(leaf),
                    std::fs::Permissions::from_mode(0o700),
                )
                .unwrap();
            }
            let stdout = private.path().join("stdout");
            let stderr = private.path().join("stderr");
            let output = |path: &std::path::Path| {
                std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(path)
                    .unwrap()
            };
            let name = thread::current().name().unwrap().to_owned();
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .env_clear()
                .env(CHILD, "1")
                .env("PATH", "/usr/bin:/bin")
                .env("HOME", private.path().join("home"))
                .env("XDG_RUNTIME_DIR", private.path().join("runtime"))
                .env("XDG_CONFIG_HOME", private.path().join("config"))
                .env("XDG_DATA_HOME", private.path().join("data"))
                .env("TMPDIR", private.path().join("tmp"))
                .args(["--exact", &name, "--nocapture", "--test-threads=1"])
                .stdin(std::process::Stdio::null())
                .stdout(output(&stdout))
                .stderr(output(&stderr))
                .spawn()
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(90);
            let status = loop {
                if let Some(status) = child.try_wait().unwrap() {
                    break status;
                }
                if Instant::now() >= deadline
                    || [&stdout, &stderr]
                        .iter()
                        .any(|path| std::fs::metadata(path).unwrap().len() > 1024 * 1024)
                {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("private target-removal fixture exceeded its finite bound");
                }
                thread::sleep(Duration::from_millis(5));
            };
            let read = |path: &std::path::Path| {
                let mut bytes = Vec::new();
                std::fs::File::open(path)
                    .unwrap()
                    .take(1024 * 1024 + 1)
                    .read_to_end(&mut bytes)
                    .unwrap();
                assert!(bytes.len() <= 1024 * 1024);
                String::from_utf8(bytes).unwrap()
            };
            let stdout = read(&stdout);
            let stderr = read(&stderr);
            assert!(
                status.success(),
                "private target fixture: {status}\n{stdout}\n{stderr}"
            );
            assert!(
                stdout
                    .lines()
                    .any(|line| line == format!("test {name} ... ok"))
            );
            assert!(
                stdout
                    .lines()
                    .any(|line| line.starts_with("test result: ok. 1 passed; 0 failed;"))
            );
            return;
        }
        account_handoff_case(true, true);
    }

    fn account_handoff_case(explicit: bool, removed_target: bool) {
        use std::os::unix::fs::PermissionsExt;
        struct Idle;
        impl Adapter for Idle {
            fn account_switch_ready(&self) -> bool {
                true
            }
            fn initialize(&mut self) -> Result<Step, String> {
                Ok(Step::default())
            }
            fn command(&mut self, _: Command) -> Result<Step, String> {
                Err("No work is issued by this fixture".into())
            }
            fn receive(&mut self, _: Value) -> Result<Step, String> {
                Ok(Step {
                    suppress_current_notification: false,
                    notification_preview: None,
                    writes: Vec::new(),
                    events: vec![
                        Event::Ready(true),
                        Event::Session {
                            id: "12345678-1234-4234-8234-123456789abc".into(),
                            title: None,
                        },
                    ],
                })
            }
        }
        let directory = tempfile::tempdir().unwrap();
        let cwd = directory.path().join("project");
        std::fs::create_dir(&cwd).unwrap();
        let cwd = cwd.canonicalize().unwrap();
        let provider = directory.path().join("claude");
        let conversation = "12345678-1234-4234-8234-123456789abc";
        std::fs::write(&provider, format!(r#"#!/bin/sh
printf '%s\n' '{{"type":"system","subtype":"init","session_id":"{conversation}"}}'
while IFS= read -r frame; do
    request=$(printf '%s' "$frame" | sed -n 's/.*"request_id":"\([^"]*\)".*/\1/p')
    if [ -n "$request" ]; then
        printf '{{"type":"control_response","response":{{"subtype":"success","request_id":"%s","response":{{"commands":[],"models":[]}}}}}}\n' "$request"
    fi
done
"#)).unwrap();
        std::fs::set_permissions(&provider, std::fs::Permissions::from_mode(0o700)).unwrap();
        let original_home = directory.path().join("original");
        let target_owner = removed_target.then(|| {
            let homes = crate::account::home::Homes::installation().unwrap();
            let store = crate::account::registry::Store::installation().unwrap();
            let (key, home) = store
                .create_pending(&homes, Provider::Claude, 1, || {
                    homes.create(Provider::Claude, None)
                })
                .unwrap();
            (store, homes, key, home)
        });
        let alternate_home = target_owner
            .as_ref()
            .map(|(_, _, _, home)| home.clone())
            .unwrap_or_else(|| directory.path().join("alternate"));
        std::fs::create_dir_all(&alternate_home).unwrap();
        let project: String = cwd
            .to_str()
            .unwrap()
            .chars()
            .map(|character| {
                if character.is_ascii_alphanumeric() {
                    character
                } else {
                    '-'
                }
            })
            .collect();
        let transcript = original_home
            .join("projects")
            .join(&project)
            .join(format!("{conversation}.jsonl"));
        std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        std::fs::write(&transcript, "{}\n").unwrap();
        let proof = HostSessionProof {
            host_session_id: "account-handoff".into(),
            boot_id: "boot".into(),
            host_incarnation: "incarnation".into(),
        };
        let host = Arc::new(crate::host::state::HostLock::new(HostState::new(
            proof.clone(),
        )));
        let mut source = Session {
            id: TerminalId::new(),
            owned_codex_listener: None,
            native_continuity: Default::default(),
            proof: Some(proof),
            terminal: None,
            launch: chat::Launch {
                provider: Provider::Claude,
                session_id: Some(conversation.into()),
                recipe: LaunchRecipe {
                    program: provider.to_str().unwrap().into(),
                    arguments: Vec::new(),
                    working_directory: cwd,
                    environment: BTreeMap::from([
                        ("HOME".into(), directory.path().to_str().unwrap().into()),
                        (
                            "CLAUDE_CONFIG_DIR".into(),
                            original_home.to_str().unwrap().into(),
                        ),
                        (crate::account::routing::AUTOMATIC_MARKER.into(), "1".into()),
                    ]),
                    unset_environment: Default::default(),
                },
            },
        };
        if explicit {
            source
                .launch
                .recipe
                .environment
                .remove(crate::account::routing::AUTOMATIC_MARKER);
        }
        let mut process_recipe = source.launch.recipe.clone();
        process_recipe.program = "/bin/cat".into();
        if removed_target {
            // The isolated negative case never starts a replacement. Its only
            // real provider also has a natural finite deadline if the caller
            // fixture itself fails before its explicit Close.
            process_recipe.program = "/usr/bin/python3".into();
            process_recipe.arguments = vec![
                "-c".into(),
                r#"import os, select, time
end = time.monotonic() + 20
while time.monotonic() < end:
    ready, _, _ = select.select([0], [], [], max(0, end - time.monotonic()))
    if not ready:
        break
    data = os.read(0, 65536)
    if not data:
        break
    os.write(1, data)
"#
                .into(),
            ];
        }
        let spec = ProcessSpec::from(&process_recipe);
        let mut initial = Step {
            suppress_current_notification: false,
            notification_preview: None,
            writes: vec![json!({"ready": true})],
            events: vec![
                Event::Ready(true),
                Event::Session {
                    id: conversation.into(),
                    title: None,
                },
            ],
        };
        let store = chat::runtime::preflight(&spec, &mut initial).unwrap();
        let handle = Handle::start(
            spec,
            Box::new(Idle),
            initial,
            store,
            Some(conversation.into()),
            |_| {},
        )
        .unwrap();
        {
            let mut state = host.lock().unwrap();
            state.register_client(1);
            state.chats.insert(
                source.id,
                Record {
                    execution_instance: uuid::Uuid::nil(),
                    console_recipe: None,
                    paired_terminal: None,
                    session: source.clone(),
                    handle: handle.clone(),
                    source_process: None,
                    subscribers: BTreeMap::from([(1, false)]),
                    observations: BTreeMap::new(),
                    completion_observed: false,
                    returning_terminal: false,
                },
            );
        }
        let mut replacement = source.clone();
        replacement.id = TerminalId::new();
        replacement.proof = None;
        replacement.launch.recipe.environment.insert(
            "CLAUDE_CONFIG_DIR".into(),
            alternate_home.to_str().unwrap().into(),
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        while handle.confirmed_session_id().is_none() {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(10));
        }
        if let Some((store, homes, key, _)) = target_owner {
            let original = handle.provider_process().unwrap();
            store
                .remove_home(&homes, Provider::Claude, key, false)
                .unwrap();
            assert!(!alternate_home.exists());
            let failure = recover_account(
                &host,
                1,
                source.clone(),
                replacement,
                None,
                Default::default(),
                true,
            )
            .unwrap_err();
            assert_eq!(failure.code, ProtocolErrorCode::AccountSwitchUnavailable);
            assert!(failure.message.contains("removed"), "{}", failure.message);
            assert!(!failure.retryable);
            assert!(!handle.is_completed());
            assert!(
                original.is_current(),
                "the same healthy original survives target refusal"
            );
            assert_eq!(std::fs::read(&transcript).unwrap(), b"{}\n");
            assert!(
                !alternate_home.exists(),
                "no target history or home was recreated"
            );
            {
                let state = host.lock().unwrap();
                assert!(state.chat_spawning.is_empty());
                assert_eq!(state.chats.len(), 1);
                assert!(state.chats[&source.id].handle.same_actor(&handle));
            }
            process(&host, 1, ClientRequest::ChatClose { chat_id: source.id }).unwrap();
            assert!(handle.is_completed());
            assert!(!original.is_current());
            return;
        }
        let generation = handle.snapshot(None).unwrap().generation;
        let original_process = handle.provider_process().unwrap();
        let capacity = {
            let mut state = host.lock().unwrap();
            let capacity = state.native_chat_capacity;
            state.native_chat_capacity = capacity::usage(&state);
            capacity
        };
        let failure = recover_account(
            &host,
            1,
            source.clone(),
            replacement.clone(),
            Some(generation),
            Default::default(),
            explicit,
        )
        .unwrap_err();
        assert_eq!(failure.code, ProtocolErrorCode::AccountSwitchUnavailable);
        assert!(failure.message.contains("capacity reached"));
        assert!(!failure.retryable);
        assert!(original_process.is_current());
        assert!(!handle.is_completed());
        assert_eq!(std::fs::read(&transcript).unwrap(), b"{}\n");
        assert!(!alternate_home.join("projects").exists());
        {
            let mut state = host.lock().unwrap();
            assert!(state.chat_spawning.is_empty());
            assert_eq!(state.chats.len(), 1);
            assert!(state.chats[&source.id].handle.same_actor(&handle));
            state.native_chat_capacity = capacity;
        }
        let mut foreign = source.clone();
        foreign.proof.as_mut().unwrap().host_incarnation = "foreign".into();
        assert!(account_switch_source(&host.lock().unwrap(), &foreign).is_err());
        assert!(!handle.is_completed());
        let observed = process(
            &host,
            1,
            ClientRequest::ChatAccountSwitchGeneration {
                source: source.clone(),
            },
        )
        .unwrap();
        assert!(
            matches!(observed, ServerResponse::ChatAccountSwitchGeneration { generation: Some(actual) } if actual == generation)
        );

        let mut pinned = source.clone();
        pinned
            .launch
            .recipe
            .environment
            .remove(crate::account::routing::AUTOMATIC_MARKER);
        let mut pinned_replacement = replacement.clone();
        pinned_replacement
            .launch
            .recipe
            .environment
            .remove(crate::account::routing::AUTOMATIC_MARKER);
        assert!(
            recover_account(
                &host,
                1,
                pinned,
                pinned_replacement,
                Some(generation),
                Default::default(),
                false,
            )
            .unwrap_err()
            .message
            .contains("pinned")
        );
        let stale = recover_account(
            &host,
            1,
            source.clone(),
            replacement.clone(),
            Some(generation + 1),
            Default::default(),
            explicit,
        )
        .unwrap_err();
        assert_eq!(stale.code, ProtocolErrorCode::AccountSwitchBusy);
        assert!(stale.retryable);
        assert!(
            !alternate_home.join("projects").exists(),
            "Busy must not copy history"
        );
        assert!(!handle.is_completed());
        assert!(host.lock().unwrap().chat_spawning.is_empty());
        let original_process = handle.provider_process().unwrap();
        let (original_directory, _) = handle.pinned_launch_directory().unwrap();
        // A same-spelling replacement folder is not the original actor's
        // authority. The exact healthy process and transcript remain intact.
        let cwd = std::path::Path::new(&source.launch.recipe.working_directory);
        let moved = directory.path().join("project-original");
        std::fs::rename(cwd, &moved).unwrap();
        std::fs::create_dir(cwd).unwrap();
        let failure = recover_account(
            &host,
            1,
            source.clone(),
            replacement.clone(),
            Some(generation),
            Default::default(),
            explicit,
        )
        .unwrap_err();
        assert_eq!(failure.code, ProtocolErrorCode::AccountSwitchUnavailable);
        assert!(!failure.retryable);
        assert!(original_process.is_current());
        assert!(!handle.is_completed());
        assert!(host.lock().unwrap().chat_spawning.is_empty());
        assert!(!alternate_home.join("projects").exists());
        std::fs::remove_dir(cwd).unwrap();
        std::fs::rename(&moved, cwd).unwrap();
        let response = recover_account(
            &host,
            1,
            source.clone(),
            replacement.clone(),
            if explicit { None } else { Some(generation) },
            Default::default(),
            explicit,
        )
        .unwrap();
        let ServerResponse::ChatAttached { session, snapshot } = response else {
            panic!("Expected confirmed account attachment")
        };
        assert_eq!(session.launch.session_id.as_deref(), Some(conversation));
        assert_eq!(snapshot.session_id.as_deref(), Some(conversation));
        assert_eq!(
            std::fs::read(
                alternate_home
                    .join("projects")
                    .join(project)
                    .join(format!("{conversation}.jsonl"))
            )
            .unwrap(),
            b"{}\n"
        );
        assert!(handle.is_completed());
        assert!(!original_process.is_current());
        let successor = host
            .lock()
            .unwrap()
            .chats
            .get(&session.id)
            .unwrap()
            .handle
            .clone();
        assert_eq!(
            successor.pinned_launch_directory().unwrap().0,
            original_directory
        );
        assert!(snapshot.items.iter().any(|item| matches!(&item.content,
            chat::ItemContent::Text { text, .. } if item.role == chat::Role::System && text.contains("Account switched"))));
        assert!(host.lock().unwrap().chat_spawning.is_empty());
        process(
            &host,
            1,
            ClientRequest::ChatClose {
                chat_id: session.id,
            },
        )
        .unwrap();
    }

    #[test]
    fn account_recovery_requires_provider_confirmation_not_the_resume_hint() {
        struct Confirm;
        impl Adapter for Confirm {
            fn initialize(&mut self) -> Result<Step, String> {
                Ok(Step::default())
            }
            fn command(&mut self, _: Command) -> Result<Step, String> {
                Ok(Step {
                    suppress_current_notification: false,
                    notification_preview: None,
                    writes: vec![json!({"id":"saved-conversation"})],
                    events: Vec::new(),
                })
            }
            fn receive(&mut self, value: Value) -> Result<Step, String> {
                Ok(Step {
                    suppress_current_notification: false,
                    notification_preview: None,
                    writes: Vec::new(),
                    events: vec![Event::Session {
                        id: value["id"].as_str().unwrap().to_owned(),
                        title: None,
                    }],
                })
            }
        }
        let recipe = LaunchRecipe {
            program: "/bin/cat".into(),
            arguments: Vec::new(),
            working_directory: std::env::temp_dir(),
            environment: BTreeMap::new(),
            unset_environment: Default::default(),
        };
        let spec = ProcessSpec::from(&recipe);
        let mut initial = Step::default();
        let store = chat::runtime::preflight(&spec, &mut initial).unwrap();
        let handle = Handle::start(
            spec,
            Box::new(Confirm),
            initial,
            store,
            Some("saved-conversation".into()),
            |_| {},
        )
        .unwrap();
        assert_eq!(handle.session_id().as_deref(), Some("saved-conversation"));
        assert!(
            confirm_account_conversation(&handle, "saved-conversation", Instant::now(), || false)
                .is_err()
        );
        handle.command(Command::Interrupt).unwrap();
        confirm_account_conversation(
            &handle,
            "saved-conversation",
            Instant::now() + Duration::from_secs(5),
            || false,
        )
        .unwrap();
        assert!(
            confirm_account_conversation(&handle, "another-conversation", Instant::now(), || false)
                .is_err()
        );
        assert!(
            confirm_account_conversation(&handle, "saved-conversation", Instant::now(), || true)
                .is_err()
        );
        handle.close().unwrap();
    }

    struct ActiveFixture;
    impl Adapter for ActiveFixture {
        fn initialize(&mut self) -> Result<Step, String> {
            Ok(Step::default())
        }
        fn command(&mut self, command: Command) -> Result<Step, String> {
            Ok(Step {
                suppress_current_notification: false,
                notification_preview: None,
                writes: Vec::new(),
                events: if matches!(command, Command::Interrupt) {
                    vec![Event::Status {
                        busy: false,
                        detail: String::new(),
                    }]
                } else {
                    Vec::new()
                },
            })
        }
        fn receive(&mut self, _: Value) -> Result<Step, String> {
            Ok(Step::default())
        }
    }
    #[test]
    fn client_listener_evidence_cannot_claim_an_unrelated_root() {
        let allocator = HostSessionProof {
            host_session_id: "allocator".into(),
            boot_id: "boot".into(),
            host_incarnation: "previous".into(),
        };
        let current = HostSessionProof {
            host_incarnation: "current".into(),
            ..allocator.clone()
        };
        let host = Arc::new(crate::host::state::HostLock::new(HostState::new(
            current.clone(),
        )));
        let mut session = Session {
            id: TerminalId::new(),
            native_continuity: Default::default(),
            owned_codex_listener: None,
            launch: chat::Launch {
                provider: Provider::Codex,
                recipe: LaunchRecipe {
                    unset_environment: Default::default(),
                    program: "codex".into(),
                    arguments: vec!["--remote".into(), "unix://".into()],
                    working_directory: "/".into(),
                    environment: BTreeMap::new(),
                },
                session_id: None,
            },
            proof: Some(current.clone()),
            terminal: None,
        };
        let root = std::path::Path::new("/daemon-root");
        assert!(session.claim_owned_codex_listener(root, &allocator));
        session.launch.session_id = Some("12345678-1234-4234-8234-123456789abc".into());
        // Inventory must retain allocation evidence while the actor starts.
        host.lock()
            .unwrap()
            .chat_spawning
            .insert(session.id, session.clone());
        let ServerResponse::ChatListed { sessions } =
            process(&host, 2, ClientRequest::ChatList).unwrap()
        else {
            panic!("chat inventory")
        };
        assert_eq!(sessions, vec![session.clone()]);
        let validated = client_session_in(&host, session.clone(), root);
        assert_eq!(validated.owned_codex_listener, session.owned_codex_listener);
        assert_eq!(
            validated.owned_codex_listener.as_ref().unwrap().allocator,
            allocator
        );

        let untrusted = client_session_in(
            &host,
            session.clone(),
            std::path::Path::new("/another-daemon-root"),
        );
        assert!(untrusted.owned_codex_listener.is_none());
        assert_eq!(untrusted.launch.recipe, session.launch.recipe);
        assert!(!untrusted.owns_codex_listener());

        // A daemon-owned checkpoint carries the original allocation across
        // both proof rotation and a changed daemon temporary root.
        let mut checkpointed: Session =
            serde_json::from_str(&serde_json::to_string(&session).unwrap()).unwrap();
        assert!(
            checkpointed
                .claim_owned_codex_listener(std::path::Path::new("/replacement-root"), &current)
        );
        assert_eq!(
            checkpointed.owned_codex_listener,
            session.owned_codex_listener
        );
        assert_eq!(checkpointed.launch.recipe, session.launch.recipe);
    }

    #[test]
    fn managed_cli_keeps_the_active_actor_and_close_retires_both_views() {
        let (_directory, host, _) = upgrade_fixture();
        let chat_id = TerminalId::new();
        let recipe = LaunchRecipe {
            unset_environment: Default::default(),
            program: "/bin/cat".into(),
            arguments: Vec::new(),
            working_directory: std::env::temp_dir(),
            environment: BTreeMap::new(),
        };
        let spec = ProcessSpec::from(&recipe);
        let mut initial = Step {
            suppress_current_notification: false,
            notification_preview: None,
            writes: Vec::new(),
            events: vec![Event::Status {
                busy: true,
                detail: "working".into(),
            }],
        };
        let store = chat::runtime::preflight(&spec, &mut initial).unwrap();
        let identity = "12345678-1234-4234-8234-123456789abc";
        let handle = Handle::start(
            spec,
            Box::new(ActiveFixture),
            initial,
            store,
            Some(identity.into()),
            |_| {},
        )
        .unwrap();
        let mut session = Session {
            native_continuity: Default::default(),
            owned_codex_listener: None,
            id: chat_id,
            proof: None,
            terminal: None,
            launch: chat::Launch {
                provider: Provider::Codex,
                recipe: recipe.clone(),
                session_id: Some(identity.into()),
            },
        };
        session.launch.recipe.arguments = vec![
            "--remote".into(),
            format!(
                "unix://{}/cagents-codex-{chat_id}/control.sock",
                std::env::temp_dir().display()
            ),
        ];
        assert!(
            session.claim_owned_codex_listener(&std::env::temp_dir(), host.lock().unwrap().proof())
        );
        {
            let mut state = host.lock().unwrap();
            state.register_client(1);
            state.chats.insert(
                chat_id,
                Record {
                    execution_instance: uuid::Uuid::nil(),
                    console_recipe: None,
                    paired_terminal: None,
                    session: session.clone(),
                    handle: handle.clone(),
                    source_process: None,
                    subscribers: BTreeMap::from([(1, false)]),
                    observations: BTreeMap::new(),
                    completion_observed: false,
                    returning_terminal: false,
                },
            );
        }
        assert_eq!(
            client_session_in(
                &host,
                session.clone(),
                std::path::Path::new("/another-root")
            )
            .owned_codex_listener,
            session.owned_codex_listener
        );
        let view = |client, session, recipe| {
            let (owners, closing) = {
                let mut state = host.lock().unwrap();
                let owners = paired_close::Owners::capture(&state, &state.chats[&chat_id])?;
                let closing =
                    capacity::Reservation::closing(&host, &mut state, &owners.target.source)?;
                (owners, closing)
            };
            codex_terminal_view(&host, client, chat_id, owners, &closing, session, recipe)
        };
        let first = view(1, session.clone(), recipe.clone()).unwrap();
        let ServerResponse::ChatTerminal {
            session: terminal,
            snapshot,
            ..
        } = first
        else {
            panic!("CLI view missing")
        };
        assert!(!snapshot.exited);
        assert!(!handle.is_completed());
        let worker = host.lock().unwrap().terminals[&terminal.id].handle.clone();
        let second = view(1, session.clone(), recipe.clone()).unwrap();
        assert!(
            matches!(second, ServerResponse::ChatTerminal { snapshot, .. } if !snapshot.exited)
        );
        assert!(
            host.lock().unwrap().terminals[&terminal.id]
                .handle
                .same_worker(&worker)
        );
        worker.shutdown().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !matches!(
            host.lock()
                .unwrap()
                .descriptor(1, terminal.id)
                .unwrap()
                .session
                .continuity,
            crate::model::TerminalContinuity::RelaunchAvailable { .. }
        ) {
            assert!(Instant::now() < deadline, "terminal exit was not published");
            thread::sleep(Duration::from_millis(10));
        }
        let mut retained = {
            let mut state = host.lock().unwrap();
            state.register_client(2);
            let mut retained = state.chats[&chat_id].session.clone();
            retained.proof = Some(state.proof().clone());
            retained
        };
        let valid_proof = retained.proof.take();
        assert!(
            start(
                &host,
                2,
                retained.clone(),
                None,
                false,
                None,
                Default::default()
            )
            .is_err()
        );
        retained.proof = valid_proof;
        assert!(matches!(
            start(&host, 2, retained, None, false, None, Default::default()).unwrap(),
            ServerResponse::ChatAttached { .. }
        ));
        assert!(
            host.lock().unwrap().chats[&chat_id]
                .handle
                .same_actor(&handle)
        );
        view(2, session, recipe).unwrap();
        let replacement = host.lock().unwrap().terminals[&terminal.id].handle.clone();
        assert!(!replacement.same_worker(&worker));
        // A recreated terminal can have the same public ID, launch recipe and
        // daemon proof. A stale private pairing must not authorize its close.
        host.lock()
            .unwrap()
            .chats
            .get_mut(&chat_id)
            .unwrap()
            .paired_terminal = Some(worker.clone());
        assert!(process(&host, 2, ClientRequest::ChatClose { chat_id }).is_err());
        assert!(!replacement.is_completed());
        assert!(host.lock().unwrap().chats.contains_key(&chat_id));
        host.lock()
            .unwrap()
            .chats
            .get_mut(&chat_id)
            .unwrap()
            .paired_terminal = Some(replacement.clone());
        {
            let mut state = host.lock().unwrap();
            let terminal = state.terminals.get_mut(&terminal.id).unwrap();
            let mut observed = terminal.session.launch.clone();
            observed.program = "codex".into();
            observed.arguments = vec!["resume".into(), identity.into()];
            terminal.session.resume_hint =
                crate::model::agent_recovery::hint_from_launch(&observed);
            assert!(terminal.session.resume_hint.is_some());
        }
        process(&host, 2, ClientRequest::ChatClose { chat_id }).unwrap();
        assert!(handle.is_completed());
        assert!(worker.is_completed());
        assert!(replacement.is_completed());
        assert!(!host.lock().unwrap().terminals.contains_key(&terminal.id));
        assert!(!host.lock().unwrap().chats.contains_key(&chat_id));
    }
    impl Adapter for Echo {
        fn initialize(&mut self) -> Result<Step, String> {
            Ok(Step::default())
        }
        fn command(&mut self, _: Command) -> Result<Step, String> {
            Ok(Step {
                suppress_current_notification: false,
                notification_preview: None,
                writes: vec![json!({"tick": true})],
                events: Vec::new(),
            })
        }
        fn receive(&mut self, _: Value) -> Result<Step, String> {
            self.0 = !self.0;
            Ok(Step {
                suppress_current_notification: false,
                notification_preview: None,
                writes: Vec::new(),
                events: vec![Event::Waiting(self.0)],
            })
        }
    }

    /// Closing a live chat reports its teardown - the provider's exit is an
    /// activity change - to every attached client, the closer included, before
    /// the close is answered. The control client attaches only to close, and
    /// must still see the close succeed and its connection stay usable.
    #[test]
    fn a_control_client_closes_a_live_chat_it_attached_to() {
        control_client_closes_chat(false, false);
    }

    #[test]
    fn a_control_client_waits_for_slow_provider_interruption_before_closing() {
        control_client_closes_chat(true, false);
    }

    #[test]
    fn returning_to_terminal_waits_for_provider_interruption() {
        control_client_closes_chat(true, true);
    }

    fn control_client_closes_chat(slow: bool, return_terminal: bool) {
        let directory = tempfile::tempdir().unwrap();
        let cli = directory.path().join("claude");
        std::fs::write(&cli, "#!/bin/sh\nexec cat\n").unwrap();
        std::fs::set_permissions(&cli, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        struct Interruptible;
        impl Adapter for Interruptible {
            fn initialize(&mut self) -> Result<Step, String> {
                Ok(Step::default())
            }
            fn command(&mut self, command: Command) -> Result<Step, String> {
                assert_eq!(command, Command::Interrupt);
                Ok(Step {
                    suppress_current_notification: false,
                    notification_preview: None,
                    writes: vec![json!({"interrupt": true})],
                    events: Vec::new(),
                })
            }
            fn receive(&mut self, value: Value) -> Result<Step, String> {
                Ok(Step {
                    suppress_current_notification: false,
                    notification_preview: None,
                    writes: Vec::new(),
                    events: vec![if value.get("ready").is_some() {
                        Event::Session {
                            id: "12345678-1234-4234-8234-123456789abc".into(),
                            title: None,
                        }
                    } else {
                        Event::Status {
                            busy: false,
                            detail: "Interrupted".into(),
                        }
                    }],
                })
            }
        }
        let proof = HostSessionProof {
            host_session_id: HostSessionId::new("control-close"),
            boot_id: BootId::new("boot"),
            host_incarnation: HostIncarnation::new("incarnation"),
        };
        let host = Arc::new(crate::host::state::HostLock::new(HostState::new(
            proof.clone(),
        )));
        let chat_id = TerminalId::new();
        let recipe = LaunchRecipe {
            unset_environment: Default::default(),
            program: if slow { "/bin/sh" } else { "/bin/cat" }.into(),
            arguments: if slow {
                vec![
                    "-c".into(),
                    "printf '{\"ready\":true}\\n'; read -r interrupt; sleep 6; printf '{\"done\":true}\\n'; exec cat".into(),
                ]
            } else {
                Vec::new()
            },
            working_directory: std::env::temp_dir(),
            environment: BTreeMap::new(),
        };
        let spec = ProcessSpec::from(&recipe);
        let mut initial = Step::default();
        if slow {
            initial.events.push(Event::Status {
                busy: true,
                detail: "Working".into(),
            });
        }
        if return_terminal {
            initial.events.push(Event::Session {
                id: "12345678-1234-4234-8234-123456789abc".into(),
                title: None,
            });
            initial.events.push(Event::Request(chat::Request {
                kind: crate::chat::RequestKind::Approval,
                id: "pending-approval".into(),
                title: "Edit".into(),
                description: "Waiting for approval".into(),
                fields: Vec::new(),
                actions: Vec::new(),
            }));
        }
        let store = chat::runtime::preflight(&spec, &mut initial).unwrap();
        let weak = Arc::downgrade(&host);
        let handle = Handle::start(
            spec,
            if slow {
                Box::new(Interruptible) as Box<dyn Adapter>
            } else {
                Box::new(Echo(false))
            },
            initial,
            store,
            return_terminal.then(|| "12345678-1234-4234-8234-123456789abc".into()),
            move |update| changed(&weak, chat_id, update),
        )
        .unwrap();
        if return_terminal {
            let deadline = Instant::now() + Duration::from_secs(5);
            while handle.confirmed_session_id().is_none() {
                assert!(
                    Instant::now() < deadline,
                    "provider did not confirm identity"
                );
                thread::sleep(Duration::from_millis(10));
            }
        }
        let session = Session {
            native_continuity: Default::default(),
            owned_codex_listener: None,
            id: chat_id,
            launch: chat::Launch {
                provider: if return_terminal {
                    Provider::Claude
                } else {
                    Provider::Codex
                },
                recipe: if return_terminal {
                    LaunchRecipe {
                        program: cli.to_string_lossy().into_owned(),
                        arguments: Vec::new(),
                        ..recipe
                    }
                } else {
                    recipe
                },
                session_id: None,
            },
            proof: Some(proof),
            terminal: None,
        };
        host.lock().unwrap().chats.insert(
            chat_id,
            Record {
                execution_instance: uuid::Uuid::nil(),
                console_recipe: None,
                paired_terminal: None,
                session: session.clone(),
                handle: handle.clone(),
                source_process: None,
                subscribers: if return_terminal {
                    BTreeMap::from([(1, true)])
                } else {
                    BTreeMap::new()
                },
                observations: BTreeMap::new(),
                completion_observed: false,
                returning_terminal: false,
            },
        );
        let socket = directory.path().join("cagentsd.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let server_host = Arc::clone(&host);
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            super::super::client::serve(stream, 1, server_host);
        });

        let mut client = crate::control::ControlClient::connect(&socket).unwrap();
        let started = Instant::now();
        if return_terminal {
            host.lock().unwrap().install_model(
                super::super::model::Registry::at(&directory.path().join("registry.sqlite3"))
                    .unwrap(),
            );
            host.lock().unwrap().register_client(1);
            let ServerResponse::ChatTerminal {
                session: terminal, ..
            } = process(&host, 1, ClientRequest::ChatReturnTerminal { chat_id }).unwrap()
            else {
                panic!("chat did not return to terminal")
            };
            // The terminal takes over the chat's lineage and its agents.
            assert_eq!(
                super::super::agent_execution::registry(&host)
                    .unwrap()
                    .lock()
                    .unwrap()
                    .session_lineage(terminal.id)
                    .unwrap(),
                Some(crate::agent_execution::Lineage(chat_id))
            );
            assert!(
                terminal
                    .launch
                    .arguments
                    .windows(2)
                    .any(|args| args == ["--resume", "12345678-1234-4234-8234-123456789abc"])
            );
            let replacement = host.lock().unwrap().terminals.remove(&terminal.id).unwrap();
            let _ = replacement.handle.shutdown();
        } else {
            client.close_chat(session).unwrap();
        }
        if slow {
            assert!(started.elapsed() >= Duration::from_secs(6));
        }
        assert!(handle.is_completed());
        assert!(client.list_chats().unwrap().is_empty());
        drop(client);
        server.join().unwrap();
    }

    #[test]
    fn delayed_producer_callback_after_rearm_is_fenced_without_reads() {
        acknowledgement_race(false);
    }

    #[test]
    fn delayed_producer_callback_before_rearm_cannot_consume_new_acknowledgement() {
        acknowledgement_race(true);
    }

    fn acknowledgement_race(before_rearm: bool) {
        let proof = HostSessionProof {
            host_session_id: HostSessionId::new("chat-observation"),
            boot_id: BootId::new("boot"),
            host_incarnation: HostIncarnation::new("incarnation"),
        };
        let host = Arc::new(crate::host::state::HostLock::new(HostState::new(
            proof.clone(),
        )));
        let chat_id = TerminalId::new();
        let recipe = LaunchRecipe {
            unset_environment: Default::default(),
            program: "/bin/cat".into(),
            arguments: Vec::new(),
            working_directory: std::env::temp_dir(),
            environment: BTreeMap::new(),
        };
        let spec = ProcessSpec::from(&recipe);
        let mut initial = Step::default();
        let store = chat::runtime::preflight(&spec, &mut initial).unwrap();
        let (observed, pending) = crossbeam_channel::bounded(1);
        let (release, released) = crossbeam_channel::bounded(1);
        let (delivered, completed) = crossbeam_channel::bounded(1);
        let weak = Arc::downgrade(&host);
        let handle = Handle::start(
            spec,
            Box::new(Echo(false)),
            initial,
            store,
            None,
            move |update| {
                let generation = match update {
                    Update::Activity { generation, .. } if before_rearm => Some(generation),
                    Update::Observed { generation } if !before_rearm => Some(generation),
                    _ => None,
                };
                if let Some(generation) = generation {
                    // Pause on either side of consuming the producer rearm, always
                    // after the semantic event and before its unread host callback.
                    if observed.send(generation).is_ok() {
                        let _ = released.recv_timeout(Duration::from_secs(5));
                    }
                }
                let observed = matches!(update, Update::Observed { .. });
                changed(&weak, chat_id, update);
                if observed {
                    let _ = delivered.try_send(());
                }
            },
        )
        .unwrap();
        let (first, second) = {
            let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
            let first = state.register_client(1);
            let second = state.register_client(2);
            state.chats.insert(
                chat_id,
                Record {
                    execution_instance: uuid::Uuid::nil(),
                    console_recipe: None,
                    paired_terminal: None,
                    session: Session {
                        native_continuity: Default::default(),
                        owned_codex_listener: None,
                        id: chat_id,
                        launch: chat::Launch {
                            provider: Provider::Claude,
                            recipe,
                            session_id: None,
                        },
                        proof: Some(proof),
                        terminal: None,
                    },
                    handle: handle.clone(),
                    source_process: None,
                    subscribers: BTreeMap::from([(1, false), (2, false)]),
                    observations: BTreeMap::from([
                        (1, Observation::default()),
                        (2, Observation::default()),
                    ]),
                    completion_observed: false,
                    returning_terminal: false,
                },
            );
            (first, second)
        };
        handle.command(Command::Interrupt).unwrap();
        let old_generation = pending.recv_timeout(Duration::from_secs(2)).unwrap();
        // This must not deadlock on the producer's snapshot lock or hydrate any
        // transcript. Only subscriber 1 acknowledges the already-produced event.
        process(
            &host,
            1,
            ClientRequest::ChatAcknowledge { chat_id, epoch: 1 },
        )
        .unwrap();
        release.send(()).unwrap();
        completed.recv_timeout(Duration::from_secs(2)).unwrap();
        handle.command(Command::Interrupt).unwrap();
        let fresh_generation = pending.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(fresh_generation > old_generation);
        // Reordered and duplicate ACKs cannot advance the source cutoff to this
        // fresh event, nor clear a subscriber's already-delivered observation.
        for epoch in [1, 0] {
            process(&host, 1, ClientRequest::ChatAcknowledge { chat_id, epoch }).unwrap();
        }
        release.send(()).unwrap();
        completed.recv_timeout(Duration::from_secs(2)).unwrap();
        for epoch in [1, 0] {
            process(&host, 1, ClientRequest::ChatAcknowledge { chat_id, epoch }).unwrap();
        }
        changed(
            &Arc::downgrade(&host),
            chat_id,
            Update::Observed {
                generation: fresh_generation + 1,
            },
        );
        // Closing the queues makes missing or surplus observations deterministic.
        first.close_after_drain();
        second.close_after_drain();
        let drain = |queue: &super::super::delivery::DeliveryQueue| {
            let mut epochs = Vec::new();
            while let Some(envelope) = queue.pop() {
                match envelope.message {
                    ServerMessage::ChatObserved { epoch, .. } => epochs.push(epoch),
                    ServerMessage::ChatActivity { .. } => {}
                    other => panic!("hidden acknowledgement requested presentation: {other:?}"),
                }
            }
            epochs
        };
        assert_eq!(
            drain(&first),
            vec![1],
            "old callback or duplicate ACK resurrected unread"
        );
        assert_eq!(
            drain(&second),
            vec![0],
            "one subscriber's ACK must not rearm another"
        );
        assert!(
            process(
                &host,
                1,
                ClientRequest::ChatRead {
                    chat_id,
                    after_generation: None
                }
            )
            .is_err()
        );
        process(&host, 1, ClientRequest::ChatDetach { chat_id }).unwrap();
        {
            let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
            state.unregister_client(2);
            let record = state.chats.get(&chat_id).unwrap();
            assert!(record.subscribers.is_empty());
            assert!(record.observations.is_empty());
        }
        drop(pending);
        drop(release);
        handle.close().unwrap();
    }
}
