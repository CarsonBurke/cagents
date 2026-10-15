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

mod account_info;
mod directory;
pub(super) mod mailbox;
mod ownership;
pub(super) mod terminals;
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
    caller(host, authority).map(|(origin, _)| origin)
}

/// The caller's origin, keyed by its lineage, and the exact live session its
/// authority was issued to, if any.
pub(super) fn caller(
    host: &SharedHost,
    authority: &Authority,
) -> Result<(Origin, Option<TerminalId>), String> {
    let registry = registry(host)?;
    let (origin, source) = {
        let registry = registry.lock().unwrap_or_else(|p| p.into_inner());
        (
            registry.execution_origin(authority)?,
            registry.agent_authority_source(authority)?,
        )
    };
    let session = source.as_ref().map(HostedSession::id);
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
    Ok((origin, session))
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
        None => {
            let preferred = accounts.project_default_account(
                &spec.target.repository,
                spec.provider,
                homes.as_ref(),
            );
            if !accounts.automatic(spec.provider)
                || accounts
                    .project_choice(&spec.target.repository, spec.provider)
                    .is_some()
            {
                return Ok(preferred);
            }
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
                .min(u64::MAX as u128) as u64;
            let entries = crate::account::cache::Cache::installation()
                .map(|cache| cache.load(now))
                .unwrap_or_default();
            Ok(crate::account::routing::select_cached(
                &accounts,
                homes.as_ref(),
                preferred,
                &entries,
                now,
            ))
        }
    }
}

pub(super) fn submit(
    host: &SharedHost,
    authority: &Authority,
    request: Submit,
) -> Result<Accepted, String> {
    submit_with_limits(host, authority, request).map_err(|error| error.to_string())
}

pub(super) fn concurrency_limits() -> Result<crate::agent_execution::ConcurrencyLimits, String> {
    let Some(path) = crate::instance::settings_path() else {
        return Ok(Default::default());
    };
    match crate::settings::load(path) {
        Ok(settings) => Ok(settings.agent_concurrency),
        Err(crate::settings::SettingsError::Io(error))
            if error.kind() == std::io::ErrorKind::NotFound =>
        {
            Ok(Default::default())
        }
        Err(error) => Err(format!("Cannot read agent concurrency settings: {error}")),
    }
}

fn submit_with_limits(
    host: &SharedHost,
    authority: &Authority,
    request: Submit,
) -> Result<Accepted, crate::agent_execution::AcceptanceError> {
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
    let limits = concurrency_limits()?;
    let selected = if let Destination::New(spec) = &request.destination {
        let mut selected = spec.clone();
        let model = registry.lock().unwrap_or_else(|p| p.into_inner()).model()?;
        selected.target.repository =
            crate::project_location::resolve(&spec.target.repository, Some(&model))?.key;
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
        .submit_agent_with_limits(&origin, &request, selected.as_ref(), limits, now())?;
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
    let worker = Arc::clone(host);
    let run = accepted.run.id;
    std::thread::Builder::new()
        .name("agent-dispatch".into())
        .spawn(move || {
            if let Err((state, diagnostic)) = dispatch(&worker, &accepted, directory) {
                finish_failure(&worker, run, state, diagnostic);
            }
        })
        .map_err(|error| {
            // No worker exists, so no provider could have received this run.
            let message = format!("Cannot create the agent dispatch worker: {error}");
            if finish_failure_registry(&registry, run, State::Failed, message.clone()) {
                announce(host, &registry);
            }
            message
        })?;
    Ok(())
}

/// The provider's own launch arguments for a new agent's settings, in the
/// spelling its native chat adapter translates. Each is applied before the
/// provider can receive a prompt, so the first turn already runs with it.
fn launch_options(spec: &crate::agent_execution::Spec) -> Vec<String> {
    use crate::agent::Provider;
    let mut arguments = Vec::new();
    if let Some(model) = &spec.model {
        arguments.extend(["--model".into(), model.clone()]);
    }
    if let Some(mode) = &spec.permission_mode {
        let flag = match spec.provider {
            Provider::Claude => "--permission-mode",
            Provider::Omp => "--approval-mode",
            Provider::Codex => "--ask-for-approval",
        };
        arguments.extend([flag.into(), mode.clone()]);
    }
    if let Some(effort) = &spec.reasoning_effort {
        arguments.extend(match spec.provider {
            Provider::Claude => ["--effort".into(), effort.clone()],
            Provider::Omp => ["--thinking".into(), effort.clone()],
            Provider::Codex => [
                "--config".into(),
                format!(
                    "{}={effort}",
                    crate::chat::protocol::codex::REASONING_EFFORT_KEY
                ),
            ],
        });
    }
    arguments
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
        session
            .launch
            .recipe
            .arguments
            .extend(launch_options(&accepted.agent.spec));
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
    // The store guard ends with this block: `announce` takes the registry lock
    // and then the host lock, so it must never run while either is held.
    let recorded = {
        let mut store = registry.lock().unwrap_or_else(|p| p.into_inner());
        (|| {
            let current = store.agent_run(run)?;
            let agent = store.agent(current.agent)?;
            if agent.closed
                || !matches!(&agent.session, Some(HostedSession::Chat(expected))
                    if expected.id == source.id && expected.proof == source.proof
                        && expected.launch.recipe == source.launch.recipe)
                || current.state.is_terminal()
            {
                return Ok(false);
            }
            store.update_agent_run(run, current.revision, update, now())?;
            Ok::<_, String>(true)
        })()
    };
    match recorded {
        Ok(true) => announce(host, &registry),
        Ok(false) => super::automations::wake(host),
        Err(error) => {
            tracing::error!(%error, %run, "cannot record provider execution outcome");
            super::automations::wake(host);
        }
    }
}

fn finish_failure(host: &SharedHost, run: RunId, state: State, diagnostic: String) {
    match registry(host) {
        Ok(registry) if finish_failure_registry(&registry, run, state, diagnostic) => {
            announce(host, &registry);
        }
        _ => super::automations::wake(host),
    }
}

/// Returns whether this call recorded the failure, so the caller announces
/// only a real transition rather than a run that had already settled.
fn finish_failure_registry(
    registry: &SharedRegistry,
    run: RunId,
    state: State,
    diagnostic: String,
) -> bool {
    let mut registry = registry.lock().unwrap_or_else(|p| p.into_inner());
    let result = registry.agent_run(run).and_then(|current| {
        if current.state.is_terminal() {
            return Ok(false);
        }
        registry
            .update_agent_run(
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
            .map(|_| true)
    });
    result.unwrap_or_else(|error| {
        tracing::error!(%error, %run, "cannot persist failed agent dispatch");
        false
    })
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
    close_as_origin(host, &origin, id, false)
}

fn close_as_origin(
    host: &SharedHost,
    origin: &Origin,
    id: AgentId,
    interrupted_only: bool,
) -> Result<Agent, String> {
    let gate = agent_gate(host, id);
    let _dispatch = gate.lock().unwrap_or_else(|p| p.into_inner());
    let registry = registry(host)?;
    let agent = registry
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .agent(id)?;
    if &agent.origin != origin {
        return Err("This caller does not own the agent".into());
    }
    if interrupted_only
        && registry
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .latest_agent_run(id)?
            .state
            != State::Interrupted
    {
        return Err("Agent is no longer interrupted; it was not closed".into());
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
                    } else if session.owned_codex_listener.is_some()
                        || matches!(
                            session.native_continuity,
                            chat::NativeContinuity::Durable { .. }
                        )
                    {
                        // Missing history can prevent actor restoration while
                        // its authenticated provider holder is still alive.
                        // Only the holder's exact receipt releases ownership.
                        super::chat::close_retained_native(host, session)?;
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
    let result = store.close_agent(origin, id)?;
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

pub(super) fn process_request(
    host: &SharedHost,
    authority: Authority,
    operation: crate::agent_execution::Operation,
) -> Result<crate::agent_execution::Reply, crate::agent_execution::AcceptanceError> {
    match operation {
        crate::agent_execution::Operation::Submit(request) => {
            submit_with_limits(host, &authority, request)
                .map(crate::agent_execution::Reply::Accepted)
        }
        other => process(host, authority, other).map_err(Into::into),
    }
}

fn close_interrupted(
    host: &SharedHost,
    authority: &Authority,
    all: bool,
    after: Option<AgentId>,
) -> Result<crate::agent_execution::CloseInterrupted, String> {
    let caller = origin(host, authority)?;
    if all && authority.token().is_some() {
        return Err("Only a user may close interrupted agents across all callers".into());
    }
    let registry = registry(host)?;
    let mut candidates = {
        let store = registry.lock().unwrap_or_else(|p| p.into_inner());
        let report = store.agent_capacity(&caller, Default::default())?;
        if all {
            report.global.holders
        } else {
            report.caller.holders
        }
    };
    candidates.retain(|holder| {
        holder.state == State::Interrupted && after.is_none_or(|previous| holder.id > previous)
    });
    candidates.sort_unstable_by_key(|holder| holder.id);
    let has_more = candidates.len() > crate::agent_execution::INTERRUPTED_CLEANUP_BATCH;
    candidates.truncate(crate::agent_execution::INTERRUPTED_CLEANUP_BATCH);
    let next = has_more.then(|| candidates.last().expect("nonempty cleanup batch").id);
    let mut result = crate::agent_execution::CloseInterrupted {
        closed: Vec::new(),
        failed: Vec::new(),
        next,
    };
    for holder in candidates {
        if holder.state != State::Interrupted {
            continue;
        }
        // Keep the authenticated caller as the gate-time authorization fence.
        // A lineage migration between the census and close must not authorize
        // this caller to close a newly reassigned conversation.
        let owner = if all {
            match registry
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .agent(holder.id)
            {
                Ok(agent) => agent.origin,
                Err(message) => {
                    result
                        .failed
                        .push(crate::agent_execution::CloseFailure::new(
                            holder.id, message,
                        ));
                    continue;
                }
            }
        } else {
            caller.clone()
        };
        match close_as_origin(host, &owner, holder.id, true) {
            Ok(closed) => result.closed.push(closed.id),
            Err(message) => result
                .failed
                .push(crate::agent_execution::CloseFailure::new(
                    holder.id, message,
                )),
        }
    }
    Ok(result)
}

pub(super) fn process(
    host: &SharedHost,
    authority: Authority,
    operation: crate::agent_execution::Operation,
) -> Result<crate::agent_execution::Reply, String> {
    use crate::agent_execution::{Operation, Reply, Summary};
    match operation {
        Operation::Capacity => {
            let origin = origin(host, &authority)?;
            let limits = concurrency_limits()?;
            let store = registry(host)?;
            let result = store
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .agent_capacity(&origin, limits)?;
            Ok(Reply::Capacity(result))
        }
        Operation::CloseInterrupted { all, after } => {
            close_interrupted(host, &authority, all, after).map(Reply::InterruptedClosed)
        }
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
        Operation::Directory(scope) => {
            directory::read(host, &authority, scope).map(Reply::Directory)
        }
        Operation::AccountInfo(query) => {
            account_info::read(host, &authority, query).map(Reply::AccountInfo)
        }
        Operation::Requests(id) => {
            let (_, _, handle) = owned_provider(host, &authority, id)?;
            Ok(Reply::Requests(displayed_requests(&handle)))
        }
        Operation::Control { agent, command } => {
            control(host, &authority, agent, command).map(Reply::Requests)
        }
        Operation::SendMessage(outgoing) => {
            mailbox::send(host, &authority, outgoing).map(Reply::Sent)
        }
        Operation::Inbox(query) => mailbox::inbox(host, &authority, query).map(Reply::Inbox),
        Operation::PeekInbox(query) => mailbox::peek(host, &authority, query).map(Reply::Inbox),
        Operation::AcknowledgeInbox(receipt) => {
            mailbox::acknowledge(host, &authority, receipt).map(|()| Reply::InboxAcknowledged)
        }
    }
}

/// The live provider of an open agent the caller owns, and the agent's
/// latest run.
fn owned_provider(
    host: &SharedHost,
    authority: &Authority,
    id: AgentId,
) -> Result<(Agent, Run, chat::runtime::Handle), String> {
    let (agent, run) = status(host, authority, id)?;
    if agent.closed {
        return Err("The agent is closed".into());
    }
    let Some(HostedSession::Chat(session)) = &agent.session else {
        return Err("The agent has no live provider to control".into());
    };
    let handle = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        state
            .chats
            .get(&session.id)
            .filter(|record| {
                !record.handle.is_completed()
                    && HostedSession::Chat(session.clone())
                        .same_process(&HostedSession::Chat(record.session.clone()))
            })
            .map(|record| record.handle.clone())
            .ok_or("The agent's provider is no longer live")?
    };
    Ok((agent, run, handle))
}

/// Pending requests as a caller may see them: a secret field's prefilled
/// value is never sent back out of the provider.
fn displayed_requests(handle: &chat::runtime::Handle) -> Vec<chat::Request> {
    let mut requests = handle.requests();
    hide_secrets(&mut requests);
    requests
}

fn hide_secrets(requests: &mut [chat::Request]) {
    for field in requests.iter_mut().flat_map(|request| &mut request.fields) {
        if field.secret {
            field.initial.clear();
        }
    }
}

/// Interrupts or answers an owned agent's provider without a console. The
/// owner acts as the agent's caller, not as a person taking it over, so the
/// agent stays under its owner's control afterwards. Both act only on the
/// owner's own active run: once a person has taken the agent over, its turns
/// and the questions they raise are that person's.
fn control(
    host: &SharedHost,
    authority: &Authority,
    id: AgentId,
    command: crate::agent_execution::Control,
) -> Result<Vec<chat::Request>, String> {
    use crate::agent_execution::Control;
    // Ownership first, so a caller cannot make the daemon keep a gate for an
    // agent it does not own.
    status(host, authority, id)?;
    let gate = agent_gate(host, id);
    let _dispatch = gate.lock().unwrap_or_else(|p| p.into_inner());
    let (_, run, handle) = owned_provider(host, authority, id)?;
    if registry(host)?
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .agent_is_interactive(id)?
    {
        return Err("A person took this agent over; its controls are theirs".into());
    }
    if run.state.is_terminal() {
        return Err(format!(
            "The agent has no active run; its latest is {:?}",
            run.state
        ));
    }
    let answered = match command {
        Control::Interrupt {
            including_subagents,
        } => {
            handle.interrupt_owned(Some(run.id), including_subagents)?;
            None
        }
        Control::Answer {
            request_id,
            action,
            answers,
        } => {
            let requests = handle.requests();
            let request = requests
                .iter()
                .find(|request| request.id == request_id)
                .ok_or_else(|| format!("No pending request {request_id}"))?;
            if !request.actions.iter().any(|choice| choice.id == action) {
                let actions: Vec<_> = request.actions.iter().map(|c| c.id.as_str()).collect();
                return Err(format!(
                    "Request {request_id} accepts the actions {}",
                    actions.join(", ")
                ));
            }
            if let Some(answer) = answers
                .iter()
                .find(|answer| !request.fields.iter().any(|field| field.id == answer.field))
            {
                return Err(format!(
                    "Request {request_id} has no field {}",
                    answer.field
                ));
            }
            handle.command(Command::Answer {
                request_id: request_id.clone(),
                action,
                answers,
            })?;
            Some(request_id)
        }
    };
    // An answered request is no longer the caller's to answer, even before the
    // provider has dropped it.
    let mut requests = displayed_requests(&handle);
    requests.retain(|request| Some(&request.id) != answered.as_ref());
    Ok(requests)
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
    let program = crate::companion::Product::Cli
        .beside_current()
        .map_err(|e| e.to_string())?;
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
    let mut retained_before = session.clone();
    retained_before.launch.session_id = handle.confirmed_session_id();
    chat::protocol::align_resume_identity(&mut retained_before.launch)?;
    retained_before.native_continuity = handle.native_continuity();
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
        predecessor: None,
    };
    let (descriptor, console_handle) = super::state::spawn_terminal_view_owned(
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
            if !state
                .chats
                .get(&session.id)
                .is_some_and(|record| record.handle.same_actor(handle))
                || !state
                    .terminals
                    .get(&session.id)
                    .is_some_and(|record| record.handle.same_worker(&console_handle))
            {
                return Err("The exact provider or console changed before pairing".into());
            }
            let record = state.chats.get_mut(&session.id).expect("checked actor");
            record.session.terminal = Some(descriptor.session.clone());
            record.paired_terminal = Some(console_handle.clone());
        }
        let mut retained = retained_before.clone();
        retained.terminal = Some(descriptor.session.clone());
        handle.update_native_source(
            retained,
            Some(descriptor.session.launch.clone()),
            console_handle.holder_identity().cloned(),
        )?;
        {
            let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
            if state
                .terminals
                .get(&session.id)
                .is_none_or(|record| !record.handle.same_worker(&console_handle))
            {
                return Err("The console changed before publishing its provider pairing".into());
            }
            let record = state
                .chats
                .get_mut(&session.id)
                .filter(|record| record.handle.same_actor(handle))
                .ok_or("The provider closed while publishing its console")?;
            record.session.terminal = Some(descriptor.session.clone());
            record.paired_terminal = Some(console_handle.clone());
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
        // The holder must retain its exact pair until terminal teardown is
        // proved. Clearing the pair first loses the cleanup owner if the daemon
        // crashes between that acknowledgment and terminal shutdown.
        let (terminal_source, proof) = {
            let state = host.lock().unwrap_or_else(|p| p.into_inner());
            let terminal = state.terminals.get(&session.id).filter(|record| {
                record.handle.same_worker(&console_handle)
                    && super::chat::matches_paired_terminal(&record.session, &descriptor.session, state.proof())
            }).ok_or_else(|| ConsoleError::Uncertain(format!(
                "{error}; the exact console is no longer registered; its pairing was retained"
            )))?;
            (terminal.session.clone(), state.proof().clone())
        };
        let cleanup = if console_handle.holder_identity().is_some() {
            console_handle.close_with_receipt(&descriptor.session, &proof)
        } else {
            console_handle.shutdown().map_err(|error| error.to_string())
        };
        if let Err(close) = cleanup {
            return Err(ConsoleError::Uncertain(format!(
                "{error}; console teardown is unconfirmed: {close}. Its exact pairing was retained."
            )));
        }
        if let Err(rollback) = handle.update_native_source(retained_before.clone(), None, None) {
            return Err(ConsoleError::Uncertain(format!(
                "{error}; console ownership rollback is unconfirmed: {rollback}. Its cleanup proof and pair were retained."
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
                record.paired_terminal = None;
            }
            if state.terminals.get(&session.id).is_some_and(|record| {
                super::chat::matches_paired_terminal(
                    &record.session,
                    &terminal_source,
                    state.proof(),
                ) && record.handle.same_worker(&console_handle)
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
    let (_, caller) = caller(host, authority)?;
    let registry = registry(host)?;
    let agent = registry
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .agent(id)?;
    let terminal = agent
        .console
        .as_ref()
        .ok_or("The console is not attached yet")?;
    // Only the console session itself, not its lineage, drives the agent.
    if agent.closed || caller != Some(terminal.id) {
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
    // Older console clients expressed interruption as a generic control.
    // Normalize it before claiming interactive ownership so a refused request
    // does not take over the agent or bypass the same dispatch safeguard.
    let command = match command {
        ConsoleCommand::Control(Command::Interrupt) => ConsoleCommand::Interrupt {
            including_subagents: false,
        },
        command => command,
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
                        name: None,
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
        ConsoleCommand::Interrupt {
            including_subagents,
        } => {
            let gate = agent_gate(host, id);
            let _dispatch = gate.lock().unwrap_or_else(|p| p.into_inner());
            handle.interrupt_owned(None, including_subagents)?;
            registry
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .claim_agent_interaction(id, None)?;
            Ok(Reply::Console(None))
        }
        ConsoleCommand::Control(command) => {
            if !matches!(
                command,
                Command::Answer { .. }
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
                    chat::ItemContent::Delivery {
                        kind,
                        from,
                        sender,
                        summary,
                        body,
                    } => {
                        let heading = chat::delivery_heading(*kind, from, sender, summary);
                        if body.is_empty() {
                            format!("{heading}\n")
                        } else {
                            format!("{heading}:\n{body}\n")
                        }
                    }
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
            hide_secrets(&mut requests);
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
