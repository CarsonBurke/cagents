//! One execution path for interactive CLI callers and scheduled firings.
//! SQLite never runs under HostState; provider callbacks carry the accepted
//! RunId rather than consulting whichever turn happens to be current.
use super::{model::Registry, state::SharedHost};
use crate::{
    account::{Account, home::Homes, registry::Store},
    agent_execution::{
        Accepted, Agent, Authority, Destination, HostedSession, Origin, Run, Spec, State, Submit,
        Surface, Update,
    },
    chat::{self, Command, PromptMode},
    model::{AgentId, RunId, TerminalId},
    terminal_ipc::{ClientRequest, ServerResponse},
};
use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

mod ownership;
pub(super) use ownership::{
    Reconciliation, cancel_agent_run, cancel_agent_run_under_control, interactive,
    reconcile_agent_run, reconcile_agent_run_under_control,
};

// Client IDs allocated to external sockets begin at one. This subscriber owns
// only accepted execution sessions and never requests presentation delivery.
const EXECUTION_CLIENT: u64 = 0;
pub(super) type SharedRegistry = Arc<Mutex<Registry>>;

pub(super) fn registry(host: &SharedHost) -> Result<SharedRegistry, String> {
    host.lock()
        .unwrap_or_else(|p| p.into_inner())
        .execution_registry
        .clone()
        .ok_or_else(|| "The agent execution registry is unavailable".into())
}

pub(super) fn origin(host: &SharedHost, authority: &Authority) -> Result<Origin, String> {
    let registry = registry(host)?;
    let (origin, source) = {
        let registry = registry.lock().unwrap_or_else(|p| p.into_inner());
        (
            registry.execution_origin(authority)?,
            registry.agent_authority_source(authority)?,
        )
    };
    if let Some(source) = source {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        let live = match source {
            HostedSession::Chat(source) => state.chats.get(&source.id).is_some_and(|record| {
                !record.handle.is_completed()
                    && record.session.proof == source.proof
                    && record.session.launch.recipe == source.launch.recipe
                    && record.session.launch.provider == source.launch.provider
            }),
            HostedSession::Terminal(source) => {
                state.terminals.get(&source.id).is_some_and(|record| {
                    !record.handle.is_completed()
                        && record.session.launch == source.launch
                        && source.continuity.live_proof().as_ref() == Some(state.proof())
                })
            }
        };
        if !live {
            return Err("The issuing agent session is no longer live or proved adopted".into());
        }
    }
    Ok(origin)
}

pub(super) fn selected_account(spec: &Spec) -> Result<Account, String> {
    let store = Store::installation().ok_or("Account registry is unavailable")?;
    let accounts = store.load().map_err(|e| e.to_string())?;
    if accounts.is_newer() {
        return Err("Account registry needs a newer cagents version".into());
    }
    let homes = Homes::installation();
    match &spec.account {
        // OMP manages its own logins and therefore has no system row in the
        // Accounts UI. Its normal launch is still a concrete system selection,
        // including after acceptance has frozen that choice into the spec.
        Some(id) if *id == Account::system(spec.provider).id() => {
            Ok(Account::system(spec.provider))
        }
        Some(id) => accounts
            .accounts(homes.as_ref())
            .into_iter()
            .find(|account| account.provider == spec.provider && account.id() == *id)
            .ok_or_else(|| "The selected provider account was removed".into()),
        None => Ok(accounts.project_default_account(
            &spec.target.repository,
            spec.provider,
            homes.as_ref(),
        )),
    }
}

pub(super) fn submit(
    host: &SharedHost,
    authority: &Authority,
    request: Submit,
) -> Result<Accepted, String> {
    request.validate()?;
    let origin = origin(host, authority)?;
    let registry = registry(host)?;
    if let Some(previous) = registry
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .previous_agent_acceptance(&origin, &request)?
    {
        return Ok(previous);
    }
    let selected = if let Destination::New(spec) = &request.destination {
        let mut selected = spec.clone();
        selected.target.repository =
            crate::workspace_model::canonical_repository(&spec.target.repository)?;
        selected.target.directory =
            std::fs::canonicalize(&spec.target.directory).map_err(|e| e.to_string())?;
        if !selected.target.directory.is_dir() {
            return Err("The agent working directory is not a directory".into());
        }
        selected.account = Some(selected_account(&selected)?.id());
        Some(selected)
    } else {
        None
    };
    let accepted = registry
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .submit_agent_with_spec(&origin, &request, selected.as_ref(), now())?;
    announce(host, &registry);
    dispatch_accepted(host, accepted.clone())?;
    Ok(accepted)
}

/// Accepts only an exact committed record. The once-only dispatch claim itself
/// commits before the worker can spawn a process or submit a provider prompt.
/// Schedulers call this after committing their atomic firing/acceptance row.
pub(super) fn dispatch_accepted(host: &SharedHost, accepted: Accepted) -> Result<(), String> {
    dispatch_accepted_with_directory(host, accepted, None)
}

pub(super) fn dispatch_accepted_in_directory(
    host: &SharedHost,
    accepted: Accepted,
    directory: Arc<std::fs::File>,
) -> Result<(), String> {
    dispatch_accepted_with_directory(host, accepted, Some(directory))
}

fn dispatch_accepted_with_directory(
    host: &SharedHost,
    accepted: Accepted,
    directory: Option<Arc<std::fs::File>>,
) -> Result<(), String> {
    let registry = registry(host)?;
    if !registry
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .claim_agent_dispatch(&accepted, now())?
    {
        return Ok(());
    }
    let host = Arc::clone(host);
    let run = accepted.run.id;
    std::thread::Builder::new()
        .name("agent-dispatch".into())
        .spawn(move || {
            if let Err((state, diagnostic)) = dispatch(&host, &accepted, directory) {
                finish_failure(&host, run, state, diagnostic);
            }
        })
        .map_err(|error| {
            // No worker exists, so no provider could have received this run.
            let message = format!("Cannot create the agent dispatch worker: {error}");
            finish_failure_registry(&registry, run, State::Failed, message.clone());
            message
        })?;
    Ok(())
}

fn dispatch(
    host: &SharedHost,
    accepted: &Accepted,
    directory: Option<Arc<std::fs::File>>,
) -> Result<(), (State, String)> {
    let before = |message| (State::Failed, message);
    let control = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .control_gate
        .clone();
    let _control = control.read().unwrap_or_else(|p| p.into_inner());
    super::client::ensure_control_open(host).map_err(|error| before(error.message))?;
    let gate = agent_gate(host, accepted.agent.id);
    let _dispatch = gate.lock().unwrap_or_else(|p| p.into_inner());
    let registry = registry(host).map_err(before)?;
    let mut context = Arc::new(chat::protocol::ExecutionContext::new(directory));
    context
        .verify_directory(&accepted.agent.spec.target.directory)
        .map_err(before)?;
    registry
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .validate_agent_dispatch(accepted)
        .map_err(before)?;
    if let Some(identity) = context.directory_identity().map_err(before)? {
        registry
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .pin_agent_directory(accepted.agent.id, identity)
            .map_err(before)?;
    } else if let Some(identity) = registry
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .agent_directory(accepted.agent.id)
        .map_err(before)?
    {
        use std::os::unix::fs::OpenOptionsExt;
        let directory = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&accepted.agent.spec.target.directory)
            .map_err(|error| before(error.to_string()))?;
        context = Arc::new(chat::protocol::ExecutionContext::new(Some(Arc::new(
            directory,
        ))));
        if context.directory_identity().map_err(before)? != Some(identity) {
            return Err(before(
                "The approved agent directory changed before provider dispatch".into(),
            ));
        }
        context
            .verify_directory(&accepted.agent.spec.target.directory)
            .map_err(before)?;
    }
    if matches!(accepted.agent.origin, Origin::Automation { .. })
        && !registry
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .agent_dispatch_is_authorized(accepted.agent.id, accepted.run.id)
            .map_err(before)?
    {
        return Err(before(
            "Interactive work took ownership of this agent; automatic dispatch was cancelled"
                .into(),
        ));
    }
    let request = registry
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .agent_request(accepted.run.id)
        .map_err(before)?;
    let (session, handle) = if let Some(HostedSession::Chat(session)) = &accepted.agent.session {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        let record = state
            .chats
            .get(&session.id)
            .filter(|record| {
                record.session.proof == session.proof
                    && record.session.launch.recipe == session.launch.recipe
                    && !record.handle.is_completed()
            })
            .ok_or_else(|| {
                before(
                    "The exact hosted conversation is unavailable; its prompt was not resent"
                        .into(),
                )
            })?;
        (record.session.clone(), record.handle.clone())
    } else {
        let account = selected_account(&accepted.agent.spec).map_err(before)?;
        let mut session = chat::fresh_session(
            accepted.agent.spec.provider,
            TerminalId::new(),
            accepted.agent.spec.target.directory.clone(),
            &account,
        )
        .map_err(before)?;
        // Use each provider's existing launch policy before it can receive a
        // prompt. Post-start settings controls are unsupported by some adapters
        // and an asynchronous acknowledgement must not race the first turn.
        if let Some(model) = &accepted.agent.spec.model {
            session
                .launch
                .recipe
                .arguments
                .extend(["--model".into(), model.clone()]);
        }
        if let Some(mode) = &accepted.agent.spec.permission_mode {
            let flag = match accepted.agent.spec.provider {
                crate::agent::Provider::Claude => "--permission-mode",
                crate::agent::Provider::Omp => "--approval-mode",
                crate::agent::Provider::Codex => "--ask-for-approval",
            };
            session
                .launch
                .recipe
                .arguments
                .extend([flag.into(), mode.clone()]);
        }
        let proof = host
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .proof()
            .clone();
        session.claim_owned_codex_listener(&std::env::temp_dir(), &proof);
        session.proof = Some(proof);
        registry
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .reserve_agent_launch(accepted.agent.id, &HostedSession::Chat(session.clone()))
            .map_err(before)?;
        let intent = HostedSession::Chat(session.clone());
        let response = super::chat::start_execution(host, EXECUTION_CLIENT, session, context.clone())
            .map_err(|error| {
                // The retained owner joins startup or receives an explicit
                // helper teardown receipt. A timed-out caller is not proof.
                let cleanup = context.reconcile_cleanup();
                if matches!(cleanup, Ok(chat::protocol::LaunchOutcome::NoProvider | chat::protocol::LaunchOutcome::TeardownProved)) {
                    let cleared = registry.lock().unwrap_or_else(|p| p.into_inner())
                        .clear_quiescent_agent_launch(accepted.agent.id, &intent);
                    match cleared {
                        Ok(()) => before(error.message),
                        Err(clear) => {
                            host.lock().unwrap_or_else(|p| p.into_inner()).execution_startups
                                .insert(accepted.agent.id, (intent.clone(), context.clone()));
                            (State::Interrupted, format!("{}; cannot clear proved-quiescent launch intent: {clear}. Close the agent to retry the durable update.", error.message))
                        },
                    }
                } else {
                    host.lock().unwrap_or_else(|p| p.into_inner()).execution_startups
                        .insert(accepted.agent.id, (intent.clone(), context.clone()));
                    (State::Interrupted, format!("Provider startup did not establish confirmed ownership: {}; cleanup: {cleanup:?}. The exact cleanup owner is retained; close the agent to retry teardown. No prompt was resent.", error.message))
                }
            })?;
        let ServerResponse::ChatAttached { session, .. } = response else {
            return Err((
                State::Interrupted,
                "Provider start returned an unexpected response".into(),
            ));
        };
        let handle = host
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .chats
            .get(&session.id)
            .ok_or_else(|| {
                (
                    State::Interrupted,
                    "The new agent disappeared before attachment; its launch intent is retained"
                        .into(),
                )
            })?
            .handle
            .clone();
        let binding = registry
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .bind_agent(
                accepted.agent.id,
                None,
                HostedSession::Chat(session.clone()),
            );
        if let Err(error) = binding {
            // Teardown is synchronous and uses the same exact actor retirement
            // path as UI close. A failed teardown retains the private launch
            // identity and interrupted capacity instead of losing ownership.
            match super::chat::process(
                host,
                EXECUTION_CLIENT,
                ClientRequest::ChatClose {
                    chat_id: session.id,
                },
            ) {
                Ok(_) => {
                    return Err(before(format!(
                        "Cannot bind provider: {error}; provider teardown confirmed"
                    )));
                }
                Err(close) => {
                    return Err((
                        State::Interrupted,
                        format!(
                            "Cannot bind provider: {error}; teardown is unconfirmed: {}",
                            close.message
                        ),
                    ));
                }
            }
        }
        announce(host, &registry);
        (session, handle)
    };
    // A visible, bound startup may be closed while a provider is initializing.
    // Reacquire and revalidate before the first external prompt, rather than
    // making interactive close wait through the whole readiness deadline.
    drop(_dispatch);
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let snapshot = handle.snapshot(None).expect("unconditional snapshot");
        if snapshot.exited || handle.is_completed() {
            return Err(before("Provider exited before accepting the prompt".into()));
        }
        if snapshot.ready {
            if snapshot.busy || snapshot.waiting || !snapshot.requests.is_empty() {
                return Err(before(
                    "The hosted provider is not idle; no additional prompt was submitted".into(),
                ));
            }
            break;
        }
        if Instant::now() >= deadline {
            return Err(before(
                "Provider did not become ready before the launch deadline".into(),
            ));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _dispatch = gate.lock().unwrap_or_else(|p| p.into_inner());
    {
        let registry = registry.lock().unwrap_or_else(|p| p.into_inner());
        if registry.agent(accepted.agent.id).map_err(before)?.closed
            || registry.agent_run(accepted.run.id).map_err(before)? != accepted.run
            || (matches!(accepted.agent.origin, Origin::Automation { .. })
                && !registry
                    .agent_dispatch_is_authorized(accepted.agent.id, accepted.run.id)
                    .map_err(before)?)
        {
            return Err(before(
                "Execution ownership changed while the provider initialized".into(),
            ));
        }
    }
    if !host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .chats
        .get(&session.id)
        .is_some_and(|record| record.handle.same_actor(&handle))
    {
        return Err(before(
            "The exact provider changed before prompt dispatch".into(),
        ));
    }
    if accepted.agent.spec.surface == Surface::Terminal && accepted.agent.console.is_none() {
        create_console(host, &registry, accepted.agent.id, &session, &handle)
            .map_err(ConsoleError::run_failure)?;
    }
    context
        .verify_directory(&accepted.agent.spec.target.directory)
        .map_err(before)?;
    if matches!(accepted.agent.origin, Origin::Automation { .. }) {
        // Live interactive work may survive removal; each scheduled prompt
        // still requires its explicitly selected account to be registered.
        selected_account(&accepted.agent.spec).map_err(before)?;
    }
    // Once command submission begins an absent acknowledgement is ambiguous.
    // Only the adapter's exact correlated result may declare success.
    handle
        .command_for_run(
            accepted.run.id,
            Command::Prompt {
                text: request.prompt,
                attachments: Vec::new(),
                mode: PromptMode::Send,
            },
        )
        .map_err(|error| (State::Interrupted, error))?;
    let _ = session;
    Ok(())
}

pub(super) fn observed(
    host: &SharedHost,
    session: TerminalId,
    instance: uuid::Uuid,
    run: RunId,
    update: Update,
) {
    let source = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        let Some(record) = state
            .chats
            .get(&session)
            .filter(|record| record.execution_instance == instance)
        else {
            return;
        };
        record.session.clone()
    };
    let Ok(registry) = registry(host) else { return };
    let mut registry = registry.lock().unwrap_or_else(|p| p.into_inner());
    let result = (|| {
        let current = registry.agent_run(run)?;
        let agent = registry.agent(current.agent)?;
        if agent.closed
            || !matches!(&agent.session, Some(HostedSession::Chat(expected))
                if expected.id == source.id && expected.proof == source.proof
                    && expected.launch.recipe == source.launch.recipe)
            || current.state.is_terminal()
        {
            return Ok(());
        }
        registry.update_agent_run(run, current.revision, update, now())?;
        Ok::<_, String>(())
    })();
    if let Err(error) = result {
        tracing::error!(%error, %run, "cannot record provider execution outcome");
    }
    drop(registry);
    super::automations::wake(host);
}

fn finish_failure(host: &SharedHost, run: RunId, state: State, diagnostic: String) {
    if let Ok(registry) = registry(host) {
        finish_failure_registry(&registry, run, state, diagnostic);
    }
    super::automations::wake(host);
}
fn finish_failure_registry(
    registry: &SharedRegistry,
    run: RunId,
    state: State,
    diagnostic: String,
) {
    let mut registry = registry.lock().unwrap_or_else(|p| p.into_inner());
    let result = registry.agent_run(run).and_then(|current| {
        if current.state.is_terminal() {
            return Ok(current);
        }
        registry.update_agent_run(
            run,
            current.revision,
            Update::Finished {
                state,
                result: crate::agent_execution::ResultSummary {
                    diagnostic: Some(diagnostic),
                    ..Default::default()
                },
            },
            now(),
        )
    });
    if let Err(error) = result {
        tracing::error!(%error, %run, "cannot persist failed agent dispatch");
    }
}

pub(super) fn status(
    host: &SharedHost,
    authority: &Authority,
    id: AgentId,
) -> Result<(Agent, Run), String> {
    let origin = origin(host, authority)?;
    let registry = registry(host)?;
    let registry = registry.lock().unwrap_or_else(|p| p.into_inner());
    let agent = registry.agent(id)?;
    if agent.origin != origin {
        return Err("This caller does not own the agent".into());
    }
    let run = registry.latest_agent_run(id)?;
    Ok((agent, run))
}
fn now() -> u64 {
    chat::now_ms().unwrap_or(0)
}

fn announce(host: &SharedHost, registry: &SharedRegistry) {
    let sequence = registry
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .execution_sequence();
    if let Ok(sequence) = sequence {
        host.lock()
            .unwrap_or_else(|p| p.into_inner())
            .announce_model_edit(
                EXECUTION_CLIENT,
                sequence,
                &crate::workspace_model::ModelEdit::AgentsChanged,
            );
    }
    super::automations::wake(host);
}

pub(super) fn retired(host: &SharedHost, session: &chat::Session) -> Result<(), String> {
    // Normal interactive sessions still work without the optional registry.
    let Ok(registry) = registry(host) else {
        return Ok(());
    };
    let changed = registry
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .retire_agent_process(&HostedSession::Chat(session.clone()), now())?;
    if changed {
        announce(host, &registry);
    }
    Ok(())
}

fn agent_gate(host: &SharedHost, id: AgentId) -> Arc<Mutex<()>> {
    host.lock()
        .unwrap_or_else(|p| p.into_inner())
        .execution_gates
        .entry(id)
        .or_default()
        .clone()
}

fn close(host: &SharedHost, authority: &Authority, id: AgentId) -> Result<Agent, String> {
    let origin = origin(host, authority)?;
    let gate = agent_gate(host, id);
    let _dispatch = gate.lock().unwrap_or_else(|p| p.into_inner());
    let registry = registry(host)?;
    let agent = registry
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .agent(id)?;
    if agent.origin != origin {
        return Err("This caller does not own the agent".into());
    }
    if agent.closed {
        return Ok(agent);
    }
    let pending = registry
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .pending_agent_launch(id)?;
    if let Some(source) = agent.session.as_ref().or(pending.as_ref()) {
        match source {
            HostedSession::Chat(session) => {
                let hosted = {
                    let state = host.lock().unwrap_or_else(|p| p.into_inner());
                    state
                        .chats
                        .get(&session.id)
                        .map(|record| record.session.clone())
                };
                if let Some(current) = hosted {
                    if current.proof != session.proof
                        || current.launch.recipe != session.launch.recipe
                    {
                        return Err("The agent ownership changed before close; its successor was not stopped".into());
                    }
                    super::chat::process(
                        host,
                        EXECUTION_CLIENT,
                        ClientRequest::ChatClose {
                            chat_id: session.id,
                        },
                    )
                    .map_err(|error| error.message)?;
                } else {
                    let startup = host
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .execution_startups
                        .get(&id)
                        .cloned();
                    if let Some((expected, context)) = startup {
                        if &expected != source {
                            return Err(
                                "The startup cleanup owner does not match the retained launch"
                                    .into(),
                            );
                        }
                        match context.reconcile_cleanup()? {
                            chat::protocol::LaunchOutcome::NoProvider
                            | chat::protocol::LaunchOutcome::TeardownProved => {
                                registry
                                    .lock()
                                    .unwrap_or_else(|p| p.into_inner())
                                    .clear_quiescent_agent_launch(id, source)?;
                            }
                            chat::protocol::LaunchOutcome::Uncertain => {
                                return Err("Startup teardown remains unconfirmed; its exact cleanup owner was retained".into());
                            }
                        }
                    } else {
                        // A lost actor is not proof that its process has exited.
                        // Recovery/holder reconciliation must resolve ownership.
                        return Err("Cannot confirm teardown of the missing agent; reconcile its saved session first".into());
                    }
                }
            }
            HostedSession::Terminal(_) => {
                return Err("Structured terminal close is not available in this build".into());
            }
        }
        registry
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .retire_agent_authority(source)?;
    }
    // No dispatch can race this proof: it uses this same per-agent gate and
    // revalidates the durable open/accepted record before creating any child.
    let mut store = registry.lock().unwrap_or_else(|p| p.into_inner());
    let current = store.latest_agent_run(id)?;
    if !current.state.is_terminal() {
        store.update_agent_run(
            current.id,
            current.revision,
            Update::Finished {
                state: State::Cancelled,
                result: current.result,
            },
            now(),
        )?;
    }
    let result = store.close_agent(&origin, id)?;
    drop(store);
    host.lock()
        .unwrap_or_else(|p| p.into_inner())
        .execution_startups
        .remove(&id);
    announce(host, &registry);
    host.lock()
        .unwrap_or_else(|p| p.into_inner())
        .execution_gates
        .remove(&id);
    Ok(result)
}

pub(super) fn process(
    host: &SharedHost,
    authority: Authority,
    operation: crate::agent_execution::Operation,
) -> Result<crate::agent_execution::Reply, String> {
    use crate::agent_execution::{Operation, Reply, Summary};
    match operation {
        Operation::Console { agent, command } => console(host, &authority, agent, command),
        Operation::Submit(request) => submit(host, &authority, request).map(Reply::Accepted),
        Operation::Status(id) => {
            status(host, &authority, id).map(|(agent, run)| Reply::Status { agent, run })
        }
        Operation::Run(id) => {
            let origin = origin(host, &authority)?;
            let registry = registry(host)?;
            let registry = registry.lock().unwrap_or_else(|p| p.into_inner());
            let run = registry.agent_run(id)?;
            let agent = registry.agent(run.agent)?;
            if agent.origin != origin {
                return Err("This caller does not own the agent".into());
            }
            Ok(Reply::Status { agent, run })
        }
        Operation::List => {
            let origin = origin(host, &authority)?;
            let registry = registry(host)?;
            let registry = registry.lock().unwrap_or_else(|p| p.into_inner());
            let summaries = registry
                .agents(Some(&origin))?
                .into_iter()
                .map(|agent| {
                    let run = registry.latest_agent_run(agent.id)?;
                    Ok(Summary {
                        agent,
                        run: run.id,
                        state: run.state,
                    })
                })
                .collect::<Result<_, String>>()?;
            Ok(Reply::Listed(summaries))
        }
        Operation::Close(id) => close(host, &authority, id).map(Reply::Closed),
    }
}

enum ConsoleError {
    Known(String),
    Uncertain(String),
}

impl ConsoleError {
    fn run_failure(self) -> (State, String) {
        match self {
            Self::Known(message) => (State::Failed, message),
            Self::Uncertain(message) => (State::Interrupted, message),
        }
    }
}

impl From<String> for ConsoleError {
    fn from(message: String) -> Self {
        Self::Known(message)
    }
}

impl From<&str> for ConsoleError {
    fn from(message: &str) -> Self {
        Self::Known(message.into())
    }
}

fn create_console(
    host: &SharedHost,
    registry: &SharedRegistry,
    agent: AgentId,
    session: &chat::Session,
    handle: &chat::runtime::Handle,
) -> Result<(), ConsoleError> {
    let program = std::env::current_exe()
        .map_err(|e| e.to_string())?
        .with_file_name("cagents");
    if !program.is_file() {
        return Err("The matched cagents CLI for the console is unavailable".into());
    }
    let recipe = crate::model::LaunchRecipe {
        program: program
            .into_os_string()
            .into_string()
            .map_err(|_| "Console executable path is not UTF-8")?,
        arguments: vec!["agent".into(), "console".into(), agent.to_string()],
        working_directory: session.launch.recipe.working_directory.clone(),
        environment: Default::default(),
        unset_environment: Default::default(),
    };
    {
        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
        let record = state
            .chats
            .get_mut(&session.id)
            .filter(|record| record.handle.same_actor(handle))
            .ok_or("The provider changed before opening its console")?;
        record.console_recipe = Some(recipe.clone());
    }
    let terminal = crate::model::TerminalSession {
        id: session.id,
        launch: recipe.clone(),
        continuity: crate::model::TerminalContinuity::RelaunchAvailable {
            recipe,
            last_exit: None,
        },
        transcript_path: None,
        resume_hint: None,
        owned_codex_listener: None,
        recovery_origin: None,
    };
    let descriptor = super::state::spawn_terminal_view(
        host,
        EXECUTION_CLIENT,
        terminal,
        crate::terminal_ipc::TerminalSpawnOptions {
            initially_visible: false,
            ..Default::default()
        },
        None,
        true,
    )
    .map_err(|error| error.message)?;
    // Publish the in-memory pair before the durable view: concurrent explicit
    // close must already know which terminal belongs to this exact actor.
    let publish = (|| {
        {
            let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
            let record = state
                .chats
                .get_mut(&session.id)
                .filter(|record| record.handle.same_actor(handle))
                .ok_or("The provider closed while publishing its console")?;
            record.session.terminal = Some(descriptor.session.clone());
        }
        let mut registry = registry.lock().unwrap_or_else(|p| p.into_inner());
        let expected = registry
            .agent(agent)?
            .session
            .ok_or("Provider binding disappeared")?;
        registry.bind_agent_console(agent, &expected, descriptor.session.clone())?;
        Ok::<_, String>(())
    })();
    if let Err(error) = publish {
        // Keep the exact terminal owned until its process teardown is proved.
        // An uncertain shutdown must retain both handles for explicit cleanup.
        let terminal = host
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .terminals
            .get(&session.id)
            .filter(|record| record.session == descriptor.session)
            .map(|record| record.handle.clone());
        if let Some(terminal) = &terminal
            && let Err(close) = terminal.shutdown()
        {
            return Err(ConsoleError::Uncertain(format!(
                "{error}; console shutdown is unconfirmed: {close}. The terminal owner was retained."
            )));
        }
        {
            let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(record) = state
                .chats
                .get_mut(&session.id)
                .filter(|record| record.handle.same_actor(handle))
            {
                record.session.terminal = None;
                record.console_recipe = None;
            }
            if terminal.as_ref().is_some_and(|handle| {
                state.terminals.get(&session.id).is_some_and(|record| {
                    record.session == descriptor.session && record.handle.same_worker(handle)
                })
            }) {
                state.retire_terminal(session.id);
            }
        }
        return Err(ConsoleError::Known(error));
    }
    announce(host, registry);
    Ok(())
}

fn console(
    host: &SharedHost,
    authority: &Authority,
    id: AgentId,
    command: crate::agent_execution::ConsoleCommand,
) -> Result<crate::agent_execution::Reply, String> {
    use crate::agent_execution::{ConsoleCommand, ConsoleView, Reply};
    let caller = origin(host, authority)?;
    let registry = registry(host)?;
    let agent = registry
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .agent(id)?;
    let terminal = agent
        .console
        .as_ref()
        .ok_or("The console is not attached yet")?;
    if agent.closed
        || caller
            != (Origin::Child {
                parent: terminal.id,
            })
    {
        return Err("This session does not own the requested agent console".into());
    }
    let handle = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        let record = state
            .chats
            .get(&terminal.id)
            .filter(|record| record.owns_execution_console())
            .ok_or("The exact paired provider console is unavailable")?;
        record.handle.clone()
    };
    match command {
        ConsoleCommand::Submit {
            prompt,
            idempotency_key,
        } => {
            let gate = agent_gate(host, id);
            let _dispatch = gate.lock().unwrap_or_else(|p| p.into_inner());
            let accepted = registry
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .submit_agent(
                    &agent.origin,
                    &Submit {
                        destination: Destination::Existing(id),
                        prompt,
                        idempotency_key,
                    },
                    now(),
                )?;
            registry
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .claim_agent_interaction(id, Some(accepted.run.id))?;
            dispatch_accepted(host, accepted.clone())?;
            Ok(Reply::Accepted(accepted))
        }
        ConsoleCommand::Control(command) => {
            if !matches!(
                command,
                Command::Answer { .. }
                    | Command::Interrupt
                    | Command::SetModel { .. }
                    | Command::SetPermissionMode { .. }
            ) {
                return Err("Use an accepted agent run to submit console prompts".into());
            }
            let gate = agent_gate(host, id);
            let _dispatch = gate.lock().unwrap_or_else(|p| p.into_inner());
            registry
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .claim_agent_interaction(id, None)?;
            handle.command(command)?;
            Ok(Reply::Console(None))
        }
        ConsoleCommand::Read {
            after_generation,
            after_run,
        } => {
            let run = registry
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .latest_agent_run(id)?;
            // The durable outcome can arrive after the last transcript update.
            // A transcript-only cursor must not hide completion or token usage.
            let cursor = (after_run == Some((run.id, run.revision)))
                .then_some(after_generation)
                .flatten();
            let Some(snapshot) = handle.snapshot(cursor) else {
                return Ok(Reply::Console(None));
            };
            let mut chunks = Vec::new();
            let mut remaining = crate::agent_execution::MAX_RESULT_BYTES;
            let mut truncated = false;
            for item in snapshot.items.iter().rev() {
                let mut text = match &item.content {
                    chat::ItemContent::Text {
                        text,
                        thinking: false,
                        ..
                    } => format!("{:?}: {text}\n", item.role),
                    chat::ItemContent::Command {
                        command,
                        status,
                        output,
                        ..
                    } => format!("Command ({status:?}): {command}\n{output}\n"),
                    chat::ItemContent::Tool {
                        name,
                        status,
                        output,
                        ..
                    } => format!("Tool {name} ({status:?}): {output}\n"),
                    _ => continue,
                };
                if text.len() > remaining {
                    crate::agent_execution::truncate(&mut text, remaining);
                    truncated = true;
                }
                remaining -= text.len();
                chunks.push(text);
                if remaining == 0 {
                    break;
                }
            }
            chunks.reverse();
            let mut requests = snapshot.requests;
            for request in &mut requests {
                for field in &mut request.fields {
                    if field.secret {
                        field.initial.clear();
                    }
                }
            }
            Ok(Reply::Console(Some(ConsoleView {
                generation: snapshot.generation,
                text: chunks.concat(),
                truncated,
                requests,
                ready: snapshot.ready,
                run,
            })))
        }
    }
}

#[cfg(test)]
mod tests;
