use super::state::{ClientId, HostState, SharedHost};
#[cfg(test)]
mod account_isolation_tests;
mod native_cleanup;
mod native_recovery;
#[cfg(test)]
mod startup_tests;
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
    sync::{Arc, Mutex, Weak},
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
    if !record.subscribers.contains_key(&client) {
        return Err(ProtocolError::new(
            ProtocolErrorCode::PermissionDenied,
            "Client is not attached to this chat",
            false,
        ));
    }
    Ok(record)
}
fn changed(host: &Weak<Mutex<HostState>>, id: TerminalId, update: Update) {
    let Some(host) = host.upgrade() else {
        return;
    };
    if matches!(&update, Update::Attention(_)) {
        // Scheduled outcomes belong to the accepted run, not its temporary
        // provider process. The scheduler publishes committed run evidence.
        // Read outside HostState; a failed ownership read cannot safely turn
        // that evidence into a second, session-scoped notification.
        if let Ok(registry) = super::agent_execution::registry(&host) {
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
    let proof = state.proof().clone();
    if let Update::Attention(evidence) = &update {
        if let Some(record) = state.chats.get(&id)
            && let Some(notifications) = &state.notifications
        {
            notifications.record(crate::notifications::Event::session(
                id,
                record.session.launch.provider,
                record.session.launch.recipe.working_directory.clone(),
                evidence.clone(),
            ));
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
fn session_snapshot(record: &Record) -> ServerResponse {
    let snapshot = record
        .handle
        .snapshot(None)
        .expect("unconditional snapshot");
    let mut session = record.session.clone();
    session.launch.session_id = snapshot.session_id.clone();
    session.native_continuity = snapshot.native_continuity.clone();
    ServerResponse::ChatAttached { session, snapshot }
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
fn has_native_owner(session: &Session) -> Result<bool, ProtocolError> {
    if session.launch.provider != Provider::Codex {
        return Ok(false);
    }
    use crate::daemon::holder_registry::{self as registry, Kind};
    let directory = registry::directory(Kind::Native).map_err(unavailable)?;
    registry::evidence(&registry::socket(&directory, session.id), Kind::Native)
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
            return Ok(session_snapshot(record));
        }
    }
    if !execution_environment.0.is_empty() && (source.is_some() || session.terminal.is_some()) {
        return Err(error(
            "A terminal handoff must use the proven source environment, not a client override",
        ));
    }
    if matches!(
        session.native_continuity,
        chat::NativeContinuity::Durable { .. }
    ) || has_native_owner(&session)?
    {
        return native_recovery::restore(host, client, &session).map_err(unavailable);
    }
    let reservation_id = session.id;
    let mut source_process = None;
    let mut source_fresh = None;
    let mut source_environment = BTreeMap::new();
    let mut source_transient = None;
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
        session.launch.recipe = recipe;
        session.launch.session_id = fresh.is_none().then_some(id);
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
            return Ok(session_snapshot(record));
        }
    }
    // All adapter validation happens before a source terminal can be changed.
    // Claude reads the selected home's transcript while building its adapter.
    // Classify a removed account before that read can mask it as missing
    // history. Codex also supports a live socket attachment, whose account is
    // deliberately not reauthorized; its transport is checked below instead.
    if session.launch.provider != Provider::Codex {
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
    if let Some(owned_socket) = &owned_socket {
        // An unclean daemon exit can leave the owned socket inode behind.
        // Only remove a refused socket inside our private, user-owned directory;
        // a live listener is reused and an arbitrary endpoint is never touched.
        if owned_socket.exists() {
            use std::os::unix::fs::{FileTypeExt, MetadataExt};
            let private = owned_socket.parent().is_some_and(|directory| {
                std::fs::symlink_metadata(directory).is_ok_and(|metadata| {
                    metadata.is_dir()
                        && metadata.uid() == unsafe { libc::geteuid() }
                        && metadata.mode() & 0o777 == 0o700
                })
            });
            let socket = std::fs::symlink_metadata(&owned_socket).is_ok_and(|metadata| {
                metadata.file_type().is_socket() && metadata.uid() == unsafe { libc::geteuid() }
            });
            if private
                && socket
                && let Err(failure) = std::os::unix::net::UnixStream::connect(&owned_socket)
                && failure.kind() == std::io::ErrorKind::ConnectionRefused
            {
                std::fs::remove_file(&owned_socket).map_err(|failure| {
                    unavailable(format!("Cannot retire stopped provider socket: {failure}"))
                })?;
            }
        }
    }
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
    let process_launch = restart_launch.unwrap_or(&session.launch);
    let (mut spec, mut adapter) =
        if let Some(owned_socket) = owned_socket.filter(|path| !path.exists()) {
            chat::protocol::codex::create_with_listener(process_launch, owned_socket)
        } else {
            chat::protocol::create(process_launch)
        }
        .map_err(|message| {
            // Removal may have won after the early check while the adapter
            // was reading history. Do not hold the lifecycle lease across
            // those reads, but preserve the actionable account classification.
            check_launch_account(&session.launch)
                .err()
                .unwrap_or_else(|| error(message))
        })?;
    if !matches!(
        &spec.transport,
        chat::protocol::TransportSpec::UnixWebSocket { .. }
    ) {
        check_launch_account(&session.launch)?;
    }
    if source.is_none() {
        // A conversation with no proven source has no proven environment to
        // preserve, so it starts from the daemon's own native configuration
        // whether or not it names a stopped terminal. Requiring the absence of
        // a descriptor here left a conversation converted from a stopped agent
        // terminal with nothing but that terminal's recorded recipe, which is
        // empty for a terminal whose resume hint was discarded when its agent
        // exited.
        //
        // Standalone conversations also receive transient execution
        // configuration from the GUI, including credentials unavailable in the
        // daemon environment. Keep these values in the process spec only,
        // never the durable session. Explicit selected configuration takes
        // precedence over daemon defaults, and the recipe's own environment
        // takes precedence over both.
        if execution_environment.0.iter().any(|(name, value)| {
            !crate::model::agent_recovery::fresh_environment_key(
                session.launch.provider,
                name,
                value,
                false,
            )
        }) {
            return Err(error(
                "Native execution environment contains an unrelated or private variable",
            ));
        }
        spec.environment = sourceless_environment(
            crate::model::agent_recovery::fresh_native_environment(session.launch.provider, false),
            execution_environment.0,
            std::mem::take(&mut spec.environment),
        );
    }
    if let Some(environment) = &source_execution {
        spec.environment = environment.clone();
    }
    spec.remove_unset_environment();
    let mut initial = adapter.initialize().map_err(error)?;
    let store = chat::runtime::preflight(&spec, &mut initial).map_err(error)?;
    if terminate_source && source.is_none() {
        return Err(error("Source termination requires an exact host proof"));
    }
    let mut retired = None;
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
        if !recovering && state.chats.len() + state.chat_spawning.len() >= 128 {
            return Err(unavailable("Daemon native chat capacity reached"));
        }
        if state.chat_spawning.contains_key(&session.id)
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
        if state
            .chat_spawning
            .values()
            .any(|pending| same_conversation(&pending.launch, &session.launch))
        {
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
            session.terminal = Some(record.session.clone());
            Some(handle)
        } else {
            if state.terminals.contains_key(&session.id) && !recovering {
                return Err(error(
                    "Existing source terminal requires exact host proof; it will not be replaced implicitly",
                ));
            }
            None
        };
        if recovering {
            retired = state.chats.remove(&session.id);
        }
        session.proof = Some(state.proof().clone());
        state.chat_spawning.insert(session.id, session.clone());
        state.inventory_generation += 1;
        source_handle
    };
    // Joining a completed actor must never happen while holding the host lock:
    // its final notification may still be acquiring that same lock.
    drop(retired);
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
                || current_recipe != session.launch.recipe
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
        if let Some(context) = &spec.execution {
            context
                .verify_directory(&spec.working_directory)
                .map_err(unavailable)?;
        }
        spec.remove_unset_environment();
        let recovery_environment = spec.environment.clone();
        // Capability is transient process context, never a public launch
        // recipe. A fresh provider may delegate only as its real session.
        if !matches!(
            spec.transport,
            chat::protocol::TransportSpec::UnixWebSocket { .. }
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
            chat::protocol::TransportSpec::UnixWebSocket { .. }
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
        let handle = Handle::start_owned(
            spec,
            adapter,
            initial,
            store,
            session.clone(),
            recovery_environment,
            move |update| {
                if let Update::Execution { run, update } = update {
                    if let Some(host) = weak.upgrade() {
                        super::agent_execution::observed(
                            &host,
                            id,
                            execution_instance,
                            run,
                            update,
                        );
                    }
                } else {
                    changed(&weak, id, update);
                }
            },
        )
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
        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
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
        let response = session_snapshot(&record);
        record.handle.commit_native_installation();
        state.chats.insert(id, record);
        if let Some(context) = &execution {
            context.installed();
        }
        state.inventory_generation += 1;
        Ok(response)
    })();
    let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
    state.chat_spawning.remove(&reservation_id);
    state.inventory_generation += 1;
    result
}

/// Attach a CLI view without stopping the server-owned thread or its native
/// connection. Both descriptors retain the same daemon-owned session ID.
fn codex_terminal_view(
    host: &SharedHost,
    client: ClientId,
    chat_id: TerminalId,
    handle: &Handle,
    mut session: Session,
    recipe: crate::model::LaunchRecipe,
) -> Result<ServerResponse, ProtocolError> {
    let completed = {
        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
        let record = attached(&state, client, chat_id)?;
        if !record.handle.same_actor(handle) {
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
                snapshot: handle.snapshot(None).expect("unconditional snapshot"),
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
            if !state
                .chats
                .get(&chat_id)
                .is_some_and(|record| record.handle.same_actor(handle))
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
            session = unpaired;
            Ok(())
        })();
        if let Err(error) = release {
            let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(record) = state
                .chats
                .get_mut(&chat_id)
                .filter(|record| record.handle.same_actor(handle))
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
    let spawned = super::state::spawn_terminal_view_owned(
        host,
        client,
        terminal,
        crate::terminal_ipc::TerminalSpawnOptions {
            initially_visible: false,
            ..Default::default()
        },
        Some(handle.execution_environment()),
        true,
    );
    let (descriptor, paired_terminal) = match spawned {
        Ok(pair) => pair,
        Err(error) => {
            let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(record) = state
                .chats
                .get_mut(&chat_id)
                .filter(|record| record.handle.same_actor(handle))
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
                .filter(|record| record.handle.same_actor(handle))
                .ok_or_else(|| unavailable("The thread closed while opening its CLI view."))?;
            // Retain local cleanup ownership even if the holder acknowledgment
            // is lost. Neither owner is rediscovered through a reusable ID.
            record.session.terminal = Some(descriptor.session.clone());
            record.paired_terminal = Some(paired_terminal.clone());
        }
        let mut updated = before.clone();
        updated.terminal = Some(descriptor.session.clone());
        handle
            .update_native_source(updated, None, paired_terminal.holder_identity().cloned())
            .map_err(unavailable)?;
        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
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
            .filter(|record| record.handle.same_actor(handle))
            .ok_or_else(|| unavailable("The thread closed while publishing its CLI view."))?;
        record.session.launch.session_id = before.launch.session_id.clone();
        record.source_process = paired_terminal.observe_agent_process();
        record.returning_terminal = false;
        state.inventory_generation += 1;
        Ok(ServerResponse::ChatTerminal {
            chat_id,
            session: descriptor.session.clone(),
            snapshot: handle.snapshot(None).expect("unconditional snapshot"),
        })
    })();
    if let Err(error) = result {
        let proof = {
            let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(record) = state
                .chats
                .get_mut(&chat_id)
                .filter(|record| record.handle.same_actor(handle))
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
        handle
            .update_native_source(before, None, None)
            .map_err(|rollback| {
                unavailable(format!(
                    "{} Pair rollback is unconfirmed: {rollback}",
                    error.message
                ))
            })?;
        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
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
            .filter(|record| record.handle.same_actor(handle))
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
    if matches!(
        session.native_continuity,
        chat::NativeContinuity::Durable { .. }
    ) {
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

fn recover_account(
    host: &SharedHost,
    client: ClientId,
    source: Session,
    replacement: Session,
    execution_environment: crate::terminal_ipc::ChatExecutionEnvironment,
) -> Result<ServerResponse, ProtocolError> {
    if !chat::is_account_recovery(&source, &replacement)
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
        Ok(()) => {
            return Err(error(
                "The original account is available; retry the saved conversation normally",
            ));
        }
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
        if state.chat_spawning.contains_key(&source.id)
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
    let confirmation = confirm_account_conversation(
        &handle,
        source.launch.session_id.as_deref().unwrap(),
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
        Ok(session_snapshot(record))
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
            return if actual == expected {
                Ok(())
            } else {
                Err("The provider opened a different conversation; the saved conversation is unchanged".into())
            };
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
        ClientRequest::ChatPrepareResume { source } => {
            let proof = host
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .proof()
                .clone();
            if !source.has_valid_native_continuity() || source.launch.session_id.is_none() {
                return Err(error("Saved conversation has invalid continuity evidence"));
            }
            // A present owner must be restored, not replaced. Missing evidence
            // is never proof of a completed close.
            if has_native_owner(&source)? {
                return Ok(ServerResponse::ChatResumePrepared { session: source });
            }
            // A kernel boot change proves every process from the saved boot
            // is gone. Unlike missing runtime files within the same boot, it
            // authorizes an exact disk resume without its ephemeral receipt.
            let previous_boot = source.proof.as_ref().is_some_and(|previous| {
                previous.is_complete() && previous.boot_id != proof.boot_id
            });
            if !previous_boot
                && !chat::native_holder::closed_source(&source, &proof)
                    .map_err(unavailable)?
                    .is_some_and(|(_, paired_closed)| paired_closed)
            {
                return Err(unavailable(
                    "The old Codex owner has no confirmed close receipt. Restore its original pane and close it before reopening; no replacement was started.",
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
        ClientRequest::ChatRecoverAccount {
            source,
            replacement,
            execution_environment,
        } => recover_account(host, client, source, replacement, execution_environment),
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
            sessions.extend(state.chat_spawning.values().cloned());
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
            // A live actor remains attachable after account removal. A stopped
            // actor cannot resume with that home, so expose the same recovery
            // choice without creating a replacement process.
            let stopped_launch = {
                let state = host.lock().unwrap_or_else(|p| p.into_inner());
                validate_proof(session.proof.as_ref(), state.proof())?;
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
            Ok(session_snapshot(record))
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
            Ok(ServerResponse::ChatSnapshot {
                chat_id,
                snapshot: record.handle.snapshot(after_generation),
            })
        }
        ClientRequest::ChatReturnTerminal { chat_id } => {
            let (handle, mut session) = {
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
                let record = attached(&state, client, chat_id)?;
                (record.handle.clone(), record.session.clone())
            };
            session.launch.session_id = handle.confirmed_session_id();
            let snapshot = handle.snapshot(None).expect("unconditional snapshot");
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
                return codex_terminal_view(host, client, chat_id, &handle, session, recipe);
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
                // Another client can close this chat while the lock is released.
                // Report that instead of panicking the per-client thread, which
                // would take down every hosted PTY with it.
                let Some(record) = state.chats.get_mut(&chat_id) else {
                    return Err(unavailable(
                        "The chat was closed while returning to terminal; no replacement terminal was started",
                    ));
                };
                record.returning_terminal = true;
                record.completion_observed = false;
                state.inventory_generation += 1;
            }
            // Closing interrupts active work and waits for confirmed teardown.
            // Never start a second writer before teardown has been proved.
            handle.close().map_err(unavailable)?;
            let snapshot = handle.snapshot(None).expect("unconditional final snapshot");
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
            let descriptor = super::state::spawn_terminal_with_environment(
                host,
                client,
                terminal,
                crate::terminal_ipc::TerminalSpawnOptions {
                    initially_visible: false,
                    ..Default::default()
                },
                Some(handle.execution_environment()),
            )?;
            let retired = {
                let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
                // `returning_terminal` marks the record this request stamped while
                // it last held the lock. A concurrent close removes that key, and
                // explicit recovery re-inserts a different actor under it; neither
                // may donate its subscribers to this replacement terminal.
                let retained = state
                    .chats
                    .get(&chat_id)
                    .is_some_and(|record| record.returning_terminal);
                if !retained {
                    // The replacement has no receipt to commit and no source to be
                    // retried from, so retire it rather than leaving a PTY in the
                    // inventory that no client can ever reach. Its wake callback
                    // re-enters the host lock, so shut it down after releasing it.
                    let orphan = state.terminals.remove(&terminal_id);
                    state.inventory_generation += 1;
                    drop(state);
                    if let Some(orphan) = orphan {
                        let _ = orphan.handle.shutdown();
                    }
                    return Err(unavailable(
                        "The chat was closed while returning to terminal; the replacement terminal was retired",
                    ));
                }
                let mut record = state.chats.remove(&chat_id);
                state.chat_handoffs.insert(
                    chat_id,
                    TerminalHandoff::new(
                        session,
                        descriptor.session.clone(),
                        snapshot.generation,
                        record
                            .as_mut()
                            .map(|record| std::mem::take(&mut record.subscribers))
                            .unwrap_or_default(),
                        Instant::now() + ACKNOWLEDGEMENT_GRACE,
                    ),
                );
                state.inventory_generation += 1;
                record
            };
            drop(retired);
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
            let handle = {
                let state = host.lock().unwrap_or_else(|p| p.into_inner());
                if state.chat_handoffs.contains_key(&chat_id) {
                    return Err(error(
                        "This chat already returned to terminal; complete the retained handoff before closing its card",
                    ));
                }
                attached(&state, client, chat_id)?.handle.clone()
            };
            handle.close_for_resume().map_err(unavailable)?;
            let snapshot = handle.snapshot(None).expect("unconditional final snapshot");
            let (retained, pending_cleanup) = {
                let state = host.lock().unwrap_or_else(|p| p.into_inner());
                let retained = state
                    .chats
                    .get(&chat_id)
                    .filter(|record| record.handle.same_actor(&handle))
                    .map(|record| record.session.clone())
                    .ok_or_else(|| {
                        unavailable("The thread changed while its previous actor was closing.")
                    })?;
                (retained, state.native_closing.contains_key(&chat_id))
            };
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
                close_retained_native(host, &retained).map_err(unavailable)?;
                return Ok(ServerResponse::ChatClosed { chat_id, snapshot });
            }
            let terminal = {
                let state = host.lock().unwrap_or_else(|p| p.into_inner());
                let record = state
                    .chats
                    .get(&chat_id)
                    .filter(|record| record.handle.same_actor(&handle));
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
            if let Some((_, terminal)) = &terminal {
                terminal
                    .shutdown()
                    .map_err(|error| unavailable(error.to_string()))?;
            }
            let record = {
                let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
                if !state
                    .chats
                    .get(&chat_id)
                    .is_some_and(|record| record.handle.same_actor(&handle))
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
            Ok(ServerResponse::ChatClosed { chat_id, snapshot })
        }
        _ => Err(error("Not a native chat request")),
    }
}

pub(super) fn resolve_upgrade(
    host: &SharedHost,
    source: &Session,
) -> Result<Session, ProtocolError> {
    let state = host.lock().unwrap_or_else(|p| p.into_inner());
    let sources = state.upgrade_chats.get(&source.id).ok_or_else(|| {
        unavailable(
            state
                .native_adoption_failures
                .get(source)
                .unwrap_or("No durable upgrade replacement exists for this native chat"),
        )
    })?;
    if !sources.iter().any(|previous| {
        let saved = &previous.source;
        !(source.proof.is_none()
        || saved.proof != source.proof
        // Presentation identities can be stale on hidden cards. The old proof,
        // actor ID and immutable launch bind that descriptor to this lineage;
        // only the daemon's checkpoint chooses the replacement conversation.
        || saved.launch.provider != source.launch.provider
        || saved.launch.recipe != source.launch.recipe
        || saved.terminal.as_ref().map(|terminal| {
            (
                &terminal.id,
                &terminal.launch,
                terminal.continuity.live_proof(),
            )
        }) != source.terminal.as_ref().map(|terminal| {
            (
                &terminal.id,
                &terminal.launch,
                terminal.continuity.live_proof(),
            )
        }))
    }) {
        return Err(error(
            "Native upgrade source does not match the exact former daemon conversation",
        ));
    }
    let record = state
        .chats
        .get(&source.id)
        .ok_or_else(|| unavailable("Restored native chat is no longer hosted"))?;
    if !record.handle.owns_work() {
        return Err(unavailable(
            "Restored native provider has closed; no writer was restarted",
        ));
    }
    let mut session = record.session.clone();
    session.launch.session_id = record.handle.session_id();
    session.native_continuity = record.handle.native_continuity();
    Ok(session)
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
    let (spec, _) = chat::protocol::create(launch)?;
    let mut environment = if hosted_source {
        spec.environment
    } else {
        sourceless_environment(daemon, BTreeMap::new(), spec.environment)
    };
    launch.recipe.remove_unset_environment(&mut environment);
    Ok(environment)
}

pub(super) fn validate_upgrade_environment(
    launch: &chat::Launch,
    hosted_source: bool,
    handle: &Handle,
) -> Result<(), String> {
    if matches!(
        handle.native_continuity(),
        chat::NativeContinuity::Durable { .. }
    ) {
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

pub(super) fn restore_upgrade(
    host: &SharedHost,
    saved: &crate::upgrade::handoff::Chat,
) -> Result<(), String> {
    if matches!(
        saved.source.native_continuity,
        chat::NativeContinuity::Durable { .. }
    ) {
        native_recovery::restore(host, 0, &saved.source)?;
        return Ok(());
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut session = saved.source.clone();
    session.launch = saved.launch.clone();
    // Claude/OMP restart their process, but the conversation descriptor keeps
    // its issued recipe. Publishing the normalized resume argv instead makes
    // the next ordinary window save look like a different conversation.
    let restart_launch = matches!(session.launch.provider, Provider::Claude | Provider::Omp)
        .then_some(&saved.launch);
    if restart_launch.is_some() {
        session.launch.recipe = saved.source.launch.recipe.clone();
    }
    let source = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        session.proof = Some(state.proof().clone());
        if let Some(terminal) = session.terminal.as_mut() {
            if state.terminals.contains_key(&terminal.id) {
                *terminal = state
                    .descriptor(0, terminal.id)
                    .map_err(|error| error.message)?
                    .session;
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
    handle.close().map_err(unavailable)?;
    let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
    let removed = if state
        .chats
        .get(&terminal_id)
        .is_some_and(|record| record.handle.same_actor(handle))
    {
        state.inventory_generation += 1;
        state.chats.remove(&terminal_id)
    } else {
        None
    };
    drop(state);
    if let Some(record) = removed {
        super::agent_execution::retired(host, &record.session).map_err(unavailable)?;
    }
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
    fn removed_claude_account_is_reported_before_missing_resume_history() {
        const CHILD: &str = "CAGENTS_REMOVED_ACCOUNT_HISTORY_TEST";
        if std::env::var_os(CHILD).is_none() {
            let directory = tempfile::tempdir().unwrap();
            assert!(std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "host::chat::tests::removed_claude_account_is_reported_before_missing_resume_history", "--nocapture"])
                .env_clear().env(CHILD, "1").env("HOME", directory.path())
                .env("XDG_DATA_HOME", directory.path().join("data"))
                .env("XDG_CONFIG_HOME", directory.path().join("config"))
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
        let host = Arc::new(Mutex::new(HostState::new(HostSessionProof {
            host_session_id: HostSessionId::new("removed-history"),
            boot_id: BootId::new("boot"),
            host_incarnation: HostIncarnation::new("incarnation"),
        })));
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
        }
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
            Arc::new(Mutex::new(HostState::new(current))),
            crate::upgrade::handoff::Chat {
                source,
                launch,
                previous_sources: Vec::new(),
            },
        )
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
        let mut wrong_source = saved.source.clone();
        wrong_source.proof = restored.proof;
        assert!(resolve_upgrade(&host, &wrong_source).is_err());
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
    fn native_restart_cannot_change_the_issued_account_environment() {
        let (_directory, host, mut saved) = upgrade_fixture();
        saved
            .launch
            .recipe
            .environment
            .insert("HOME".into(), "/another/account".into());
        assert!(
            restore_upgrade(&host, &saved)
                .unwrap_err()
                .contains("differs from its issued conversation")
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
    fn account_recovery_requires_provider_confirmation_not_the_resume_hint() {
        struct Confirm;
        impl Adapter for Confirm {
            fn initialize(&mut self) -> Result<Step, String> {
                Ok(Step::default())
            }
            fn command(&mut self, _: Command) -> Result<Step, String> {
                Ok(Step {
                    writes: vec![json!({"id":"saved-conversation"})],
                    events: Vec::new(),
                })
            }
            fn receive(&mut self, value: Value) -> Result<Step, String> {
                Ok(Step {
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
        let host = Arc::new(Mutex::new(HostState::new(current.clone())));
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
        let first =
            codex_terminal_view(&host, 1, chat_id, &handle, session.clone(), recipe.clone())
                .unwrap();
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
        let second =
            codex_terminal_view(&host, 1, chat_id, &handle, session.clone(), recipe.clone())
                .unwrap();
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
        codex_terminal_view(&host, 2, chat_id, &handle, session, recipe).unwrap();
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
                writes: vec![json!({"tick": true})],
                events: Vec::new(),
            })
        }
        fn receive(&mut self, _: Value) -> Result<Step, String> {
            self.0 = !self.0;
            Ok(Step {
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
                    writes: vec![json!({"interrupt": true})],
                    events: Vec::new(),
                })
            }
            fn receive(&mut self, value: Value) -> Result<Step, String> {
                Ok(Step {
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
        let host = Arc::new(Mutex::new(HostState::new(proof.clone())));
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
        let host = Arc::new(Mutex::new(HostState::new(proof.clone())));
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
