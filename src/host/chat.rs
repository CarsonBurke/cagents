use super::state::{ClientId, HostState, SharedHost};
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
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, Weak},
    thread,
    time::{Duration, Instant},
};

pub(super) struct Record {
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

// Transition evidence is not an actor: no transcript, socket, store or execution
// environment survives a successful replacement. Keep it until the GUI confirms
// durable ownership of the terminal, never merely until a response or attach.
pub(super) struct TerminalHandoff {
    source: Session,
    terminal: crate::model::TerminalSession,
    generation: u64,
    subscribers: BTreeMap<ClientId, bool>,
}

impl TerminalHandoff {
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
fn changed(host: &Weak<Mutex<HostState>>, id: TerminalId, update: Update) {
    let Some(host) = host.upgrade() else {
        return;
    };
    let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
    let proof = state.proof().clone();
    let HostState { chats, clients, .. } = &mut *state;
    let Some(record) = chats.get_mut(&id) else {
        return;
    };
    for (&client, &visible) in &record.subscribers {
        let message = match update {
            Update::Activity { state, generation } => ServerMessage::ChatActivity {
                chat_id: id,
                state,
                generation,
            },
            Update::Presentation if visible => ServerMessage::ChatChanged { chat_id: id },
            Update::Observed { generation } if !visible => {
                let observation = record.observations.entry(client).or_default();
                if !observation.accepts(generation) {
                    continue;
                }
                ServerMessage::ChatObserved {
                    chat_id: id,
                    epoch: observation.epoch,
                }
            }
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
        Provider::Codex => "Choose the exact Codex thread ID shown by /status in the source CLI, then attach the selected conversation. No recent thread is selected automatically.",
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
    mut session: Session,
    source: Option<HostSessionProof>,
    terminate_source: bool,
    selection: Option<PendingBinding>,
    execution_environment: crate::terminal_ipc::ChatExecutionEnvironment,
) -> Result<ServerResponse, ProtocolError> {
    if !execution_environment.0.is_empty() && (source.is_some() || session.terminal.is_some()) {
        return Err(error(
            "A terminal handoff must use the proven source environment, not a client override",
        ));
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
    let (mut spec, mut adapter) = chat::protocol::create(&session.launch).map_err(error)?;
    if source.is_none() && session.terminal.is_none() {
        // Standalone conversations receive transient execution configuration from
        // the GUI, including credentials unavailable in the daemon environment.
        // Keep these values in the process spec only, never the durable session.
        // Explicit selected configuration takes precedence over daemon defaults.
        let mut environment =
            crate::model::agent_recovery::fresh_native_environment(session.launch.provider, false);
        for (name, value) in execution_environment.0 {
            if !crate::model::agent_recovery::fresh_environment_key(
                session.launch.provider,
                &name,
                &value,
                false,
            ) {
                return Err(error(
                    "Native execution environment contains an unrelated or private variable",
                ));
            }
            environment.insert(name, value);
        }
        environment.extend(std::mem::take(&mut spec.environment));
        spec.environment = environment;
    }
    if let Some(environment) = &source_execution {
        spec.environment = environment.clone();
    }
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
        if state.chat_spawning.contains_key(&session.id) {
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
            .any(|launch| same_conversation(launch, &session.launch))
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
        state
            .chat_spawning
            .insert(session.id, session.launch.clone());
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
        let weak = Arc::downgrade(host);
        let id = session.id;
        let handle = Handle::start(
            spec,
            adapter,
            initial,
            store,
            session.launch.session_id.clone(),
            move |update| changed(&weak, id, update),
        )
        .map_err(unavailable)?;
        if let Some((source, mut frozen)) = pending_fresh {
            let committed = confirm_fresh_session(
                &handle,
                &mut session.launch,
                Instant::now() + Duration::from_secs(10),
                || host.lock().unwrap_or_else(|p| p.into_inner()).shutting_down,
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
        let record = Record {
            session,
            handle,
            source_process,
            subscribers: BTreeMap::from([(client, false)]),
            observations: BTreeMap::from([(client, observation)]),
            completion_observed: false,
            returning_terminal: false,
        };
        let response = session_snapshot(&record);
        state.chats.insert(id, record);
        state.inventory_generation += 1;
        Ok(response)
    })();
    let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
    state.chat_spawning.remove(&reservation_id);
    state.inventory_generation += 1;
    result
}

pub(super) fn process(
    host: &SharedHost,
    client: ClientId,
    request: ClientRequest,
) -> Result<ServerResponse, ProtocolError> {
    match request {
        ClientRequest::ChatPreview { session, source } => preview(host, session, source),
        ClientRequest::ChatStartSelected {
            session,
            source,
            token,
        } => {
            let selection = {
                let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
                validate_proof(Some(&source), state.proof())?;
                if !state
                    .chat_bindings
                    .get(&session.id)
                    .is_some_and(|binding| binding.preview.token == token)
                {
                    return Err(error(
                        "This selection has no matching preview; preview the conversation again",
                    ));
                }
                state
                    .chat_bindings
                    .remove(&session.id)
                    .expect("matched binding")
            };
            start(
                host,
                client,
                session,
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
                    session
                })
                .collect::<Vec<_>>();
            sessions.extend(state.chat_spawning.iter().map(|(&id, launch)| Session {
                id,
                launch: launch.clone(),
                proof: Some(state.proof().clone()),
                terminal: None,
            }));
            Ok(ServerResponse::ChatListed { sessions })
        }
        ClientRequest::ChatStart {
            session,
            source,
            terminate_source,
            execution_environment,
        } => start(
            host,
            client,
            session,
            source,
            terminate_source,
            None,
            execution_environment,
        ),
        ClientRequest::ChatAttach { session } => {
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
            attached(&state, client, chat_id)?;
            let record = state.chats.get_mut(&chat_id).expect("checked chat");
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
            attached(&state, client, chat_id)?;
            let record = state.chats.get_mut(&chat_id).expect("checked chat");
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
            attached(&state, client, chat_id)?;
            let record = state.chats.get_mut(&chat_id).expect("checked chat");
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
                let state = host.lock().unwrap_or_else(|p| p.into_inner());
                if state.shutting_down {
                    return Err(unavailable("Daemon is shutting down"));
                }
                if let Some(receipt) = state.chat_handoffs.get(&chat_id) {
                    receipt.require_attached(client)?;
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
            let recipe = session.launch.terminal_recipe().ok_or_else(|| unavailable(
                "Wait for the provider's confirmed exact conversation before returning to terminal",
            ))?;
            if session.launch.provider == Provider::Codex && session.terminal.is_some() {
                return Err(error(
                    "A shared Codex source must be reattached, not replaced",
                ));
            }
            let snapshot = handle.snapshot(None).expect("unconditional snapshot");
            if snapshot.busy || !snapshot.requests.is_empty() {
                return Err(unavailable(
                    "Finish or interrupt the active exchange before returning to terminal",
                ));
            }
            let terminal_id = crate::model::TerminalId::new();
            let terminal = crate::model::TerminalSession {
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
                let record = state.chats.get_mut(&chat_id).expect("attached chat");
                record.returning_terminal = true;
                record.completion_observed = false;
                state.inventory_generation += 1;
            }
            handle.close().map_err(unavailable)?;
            let snapshot = handle.snapshot(None).expect("unconditional final snapshot");
            if snapshot.session_id != session.launch.session_id {
                return Err(unavailable(
                    "Provider identity changed during shutdown; retry the exact terminal handoff",
                ));
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
                let mut record = state.chats.remove(&chat_id).expect("attached chat");
                state.chat_handoffs.insert(
                    chat_id,
                    TerminalHandoff {
                        source: session,
                        terminal: descriptor.session.clone(),
                        generation: snapshot.generation,
                        subscribers: std::mem::take(&mut record.subscribers),
                    },
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
            handle.close().map_err(unavailable)?;
            let snapshot = handle.snapshot(None).expect("unconditional final snapshot");
            let record = {
                let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
                state.inventory_generation += 1;
                state.chats.remove(&chat_id)
            };
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
    let sources = state
        .upgrade_chats
        .get(&source.id)
        .ok_or_else(|| unavailable("No durable upgrade replacement exists for this native chat"))?;
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

pub(super) fn validate_upgrade_environment(
    session: &Session,
    launch: &chat::Launch,
    handle: &Handle,
) -> Result<(), String> {
    let (spec, _) = chat::protocol::create(launch)?;
    let mut restored = if session.terminal.is_none() {
        crate::model::agent_recovery::fresh_native_environment(launch.provider, false)
    } else {
        BTreeMap::new()
    };
    restored.extend(spec.environment);
    if !handle.execution_environment_matches(&restored) {
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
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut session = saved.source.clone();
    session.launch = saved.launch.clone();
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
    let response = start(
        host,
        0,
        session,
        source,
        false,
        selection,
        Default::default(),
    )
    .map_err(|error| error.message)?;
    let id = match response {
        ServerResponse::ChatAttached { session, .. } => session.id,
        _ => return Err("Native restore did not attach its exact session".into()),
    };
    let handle = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .chats
        .get(&id)
        .expect("restored chat")
        .handle
        .clone();
    // Startup, loaded-thread verification, and native confirmation share one deadline.
    loop {
        let snapshot = handle.snapshot(None).expect("unconditional snapshot");
        if let Some(error) = snapshot.error {
            return Err(error);
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
    let record = state
        .chats
        .get_mut(&saved.source.id)
        .expect("restored chat");
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
    fn paired_codex_upgrade_preserves_confirmed_fresh_or_switched_thread() {
        use crate::model::agent_recovery::{AgentProcess, bind_session};
        const SELECTED: &str = "12345678-1234-4234-8234-123456789abc";
        const ORIGINAL: &str = "98765432-1234-4234-8234-123456789abc";
        let recipe = LaunchRecipe {
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
    if command not in ("negotiate_protocol", "get_state", "get_available_models", "get_available_commands", "get_session_stats", "get_messages_page"):
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
                program: executable.to_string_lossy().into_owned(),
                arguments: Vec::new(),
                working_directory: directory.path().to_owned(),
                environment: BTreeMap::new(),
            },
            session_id: Some("12345678-1234-4234-8234-123456789abc".into()),
        };
        let source = Session {
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
    fn durable_terminal_commit_retires_evidence_after_terminal_retirement() {
        let (_directory, host, saved) = upgrade_fixture();
        let source_id = saved.source.id;
        let recipe = saved.launch.terminal_recipe().unwrap();
        let terminal_id = TerminalId::new();
        let terminal = crate::model::TerminalSession {
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
                TerminalHandoff {
                    source: saved.source,
                    terminal,
                    generation: 1,
                    subscribers: BTreeMap::new(),
                },
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
        let failure = super::super::state::prepare_upgrade(&host, capability).unwrap_err();
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
        let failure = super::super::state::prepare_upgrade(&host, capability).unwrap_err();
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
        let failure = super::super::state::prepare_upgrade(&host, capability).unwrap_err();
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
        let failure = super::super::state::prepare_upgrade(&host, capability).unwrap_err();
        assert!(failure.message.contains("execution environment"));
        assert!(!terminal_handle.is_completed());
        assert!(!host.lock().unwrap_or_else(|p| p.into_inner()).shutting_down);
        terminal_handle.shutdown().unwrap();
    }

    #[test]
    fn native_upgrade_restores_exact_provider_session_and_only_resolves_proven_source() {
        let (_directory, host, saved) = upgrade_fixture();
        restore_upgrade(&host, &saved).unwrap();
        let restored = resolve_upgrade(&host, &saved.source).unwrap();
        assert_eq!(restored.id, saved.source.id);
        assert_eq!(restored.launch.session_id, saved.launch.session_id);
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
        assert!(restore_upgrade(&host, &saved).is_err());
        assert!(resolve_upgrade(&host, &saved.source).is_err());
        let handle = host.lock().unwrap_or_else(|p| p.into_inner()).chats[&saved.source.id]
            .handle
            .clone();
        assert!(handle.close().is_err());
    }

    struct Echo(bool);
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
                changed(&weak, chat_id, update);
                if matches!(update, Update::Observed { .. }) {
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
                    session: Session {
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
