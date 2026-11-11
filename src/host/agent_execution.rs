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

pub(super) mod account_info;
mod directory;
pub(super) use directory::{live_sessions, named};
pub(super) mod mailbox;
mod ownership;
pub(super) mod terminals;
mod transcript;
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
            .ok_or_else(|| {
                // A home a lost account list no longer names is not removed:
                // the daemon restores it if its provider confirms the login.
                let kept = id
                    .as_str()
                    .split_once('/')
                    .and_then(|(_, key)| uuid::Uuid::parse_str(key).ok())
                    .zip(homes.as_ref())
                    .is_some_and(|(key, homes)| homes.is_unregistered(&accounts, spec.provider, key));
                if kept {
                    "The selected provider account isn't in the account list, but its files are still on this device. cagents restores it if its provider still reports a sign-in to it, so try again shortly; otherwise choose another account.".into()
                } else {
                    "The selected provider account was removed".into()
                }
            }),
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
    if let Destination::Existing(agent) = &request.destination {
        refuse_limited(host, &registry, *agent)?;
    }
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
        .submit_agent_with_spec(&origin, &request, selected.as_ref(), now())?;
    announce(host, &registry);
    dispatch_accepted(host, accepted.clone())?;
    Ok(accepted)
}

/// Refuses a follow-up run for an agent whose conversation a usage limit stops
/// until a known reset: its provider would refuse the prompt at once, and the
/// caller is better told now. A limit of unknown end is let through, since a
/// new turn is what shows whether it still holds, as after the conversation's
/// account changed.
fn refuse_limited(
    host: &SharedHost,
    registry: &SharedRegistry,
    agent: crate::model::AgentId,
) -> Result<(), String> {
    let agent = registry
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .agent(agent)?;
    let Some(HostedSession::Chat(session)) = &agent.session else {
        return Ok(());
    };
    let Some((provider, handle)) = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .chats
        .get(&session.id)
        .filter(|record| !record.handle.is_completed())
        .map(|record| (record.session.launch.provider, record.handle.clone()))
    else {
        return Ok(());
    };
    let now = now();
    let Some(limit) = directory::chat_limit(
        provider,
        &handle,
        || {
            account_info::Accounts::load(now)
                .and_then(|accounts| accounts.of(provider, &handle.execution_environment().0))
        },
        now,
    )
    .filter(|limit| limit.resets_at.is_some()) else {
        return Ok(());
    };
    let reset = limit
        .reset
        .as_deref()
        .map(|reset| format!(", resets {reset}"))
        .unwrap_or_default();
    Err(format!(
        "{} ({}{reset}), so the prompt was not sent. {}",
        crate::agent_execution::LIMITED_REFUSAL,
        limit.summary,
        crate::agent_execution::LIMITED_ADVICE,
    ))
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

/// The run whose dispatcher released the agent's gate to wait for its
/// provider to become ready. Its prompt stays undelivered until that
/// dispatcher reclaims it, so an interrupt in the meantime withdraws the run
/// instead of interrupting a turn the provider never started.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::host) struct Undelivered {
    run: RunId,
    withdrawn: bool,
}

/// Clears a dispatcher's claim if readiness polling unwinds, so a panic cannot
/// leave reconciliation waiting on a dispatcher that no longer exists.
struct ClaimUnwind<'a> {
    gate: &'a Mutex<Option<Undelivered>>,
    run: RunId,
}

impl Drop for ClaimUnwind<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.gate
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .take_if(|claim| claim.run == self.run);
        }
    }
}

/// Cancels the run whose dispatcher is still waiting for the provider, or only
/// `run` when given. The dispatcher finds its claim withdrawn and submits
/// nothing. Returns whether such a run existed, including one an earlier
/// interrupt already withdrew. Requires the agent's gate, whose value
/// `dispatch` is.
fn withdraw_undelivered(
    host: &SharedHost,
    dispatch: &mut Option<Undelivered>,
    run: Option<RunId>,
) -> Result<bool, String> {
    let Some(claim) = dispatch
        .as_mut()
        .filter(|claim| run.is_none_or(|run| claim.run == run))
    else {
        return Ok(false);
    };
    if !claim.withdrawn {
        let registry = registry(host)?;
        claim.withdrawn = true;
        // A failed write still leaves the claim withdrawn: the dispatcher
        // then reports this cancellation and finish_failure retries it.
        if finish_failure_registry(
            &registry,
            claim.run,
            State::Cancelled,
            UNDELIVERED_CANCELLATION.into(),
        ) {
            announce(host, &registry);
        }
    }
    Ok(true)
}

const UNDELIVERED_CANCELLATION: &str = "Cancelled before the provider received the prompt";

/// Waits, without the agent's gate, until a bound provider can take its
/// first prompt.
pub(in crate::host) fn await_provider_ready(
    handle: &chat::runtime::Handle,
) -> Result<(), (State, String)> {
    await_provider_ready_until(handle, Instant::now() + Duration::from_secs(30))
}

fn await_provider_ready_until(
    handle: &chat::runtime::Handle,
    deadline: Instant,
) -> Result<(), (State, String)> {
    let before = |message| (State::Failed, message);
    loop {
        // A retained snapshot is a helper RPC, and one call can miss its
        // deadline on a busy host while the helper stays healthy. Reading a
        // snapshot sends no prompt, so keep polling within the launch budget.
        // Stop at once when the attachment itself records a failure or exit.
        let snapshot = match provider_launch_snapshot(handle) {
            Ok(snapshot) => snapshot,
            Err(unconfirmed) => {
                if handle.failure().is_some() || handle.is_completed() || Instant::now() >= deadline
                {
                    return Err(unconfirmed);
                }
                std::thread::sleep(Duration::from_millis(20));
                continue;
            }
        };
        if snapshot.exited || handle.is_completed() {
            return Err(before("Provider exited before accepting the prompt".into()));
        }
        if snapshot.ready {
            if snapshot.busy || snapshot.waiting || !snapshot.requests.is_empty() {
                return Err(before(
                    "The hosted provider is not idle; no additional prompt was submitted".into(),
                ));
            }
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(before(
                "Provider did not become ready before the launch deadline".into(),
            ));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// An unavailable remote snapshot proves neither provider exit nor safe retry.
fn provider_launch_snapshot(
    handle: &chat::runtime::Handle,
) -> Result<chat::Snapshot, (State, String)> {
    handle.snapshot(None).ok_or_else(|| (State::Interrupted,
        "The exact provider could not confirm startup; its ownership was preserved and the prompt was not resent".into()))
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
    let mut dispatch = gate.lock().unwrap_or_else(|p| p.into_inner());
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
        let ended = registry
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .agent_process_ended(accepted.agent.id)
            .map_err(before)?;
        if ended {
            return Err(before(
                "This agent's process has stopped; reopen its conversation to resume it. The prompt was not sent".into(),
            ));
        }
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
        let (proof, listeners) = {
            let state = host.lock().unwrap_or_else(|p| p.into_inner());
            (state.proof().clone(), state.codex_listeners.clone())
        };
        listeners.claim(&mut session, &proof);
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
    // A visible, bound startup may be closed or interrupted while a provider
    // is initializing. Reacquire and revalidate before the first external
    // prompt, rather than making those wait through the readiness deadline.
    *dispatch = Some(Undelivered {
        run: accepted.run.id,
        withdrawn: false,
    });
    drop(dispatch);
    let ready = {
        let _unwind = ClaimUnwind {
            gate: &gate,
            run: accepted.run.id,
        };
        await_provider_ready(&handle)
    };
    let mut dispatch = gate.lock().unwrap_or_else(|p| p.into_inner());
    let claim = dispatch.take_if(|claim| claim.run == accepted.run.id);
    ready?;
    if claim.is_none_or(|claim| claim.withdrawn) {
        return Err((State::Cancelled, UNDELIVERED_CANCELLATION.into()));
    }
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
    given_work(host, session.id);
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

pub(super) fn announce(host: &SharedHost, registry: &SharedRegistry) {
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
    if host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .idle_adoptions
        .remove(&session.id)
    {
        return stopped(host, session);
    }
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

/// Daemon shutdown stopped this exact provider after confirmed teardown. Its
/// authority and unfinished runs end, but the agent keeps its pane, placement
/// and channel so the conversation resumes in place after restart.
pub(super) fn stopped(host: &SharedHost, session: &chat::Session) -> Result<(), String> {
    let Ok(registry) = registry(host) else {
        return Ok(());
    };
    let changed = registry
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .stop_agent_process(&HostedSession::Chat(session.clone()), now())?;
    if changed {
        announce(host, &registry);
    }
    Ok(())
}

/// The conversation `chat` is being given work, so it no longer only reopens
/// its agent's conversation: ending it from now on retires that agent.
pub(in crate::host) fn given_work(host: &SharedHost, chat: TerminalId) {
    host.lock()
        .unwrap_or_else(|p| p.into_inner())
        .idle_adoptions
        .remove(&chat);
}

/// Bind the agent of a resumed conversation to `successor`, the process that
/// just started under a new chat ID. The predecessor is found through the
/// lineage the successor joined when its resume was prepared, so a retry after
/// a failed start, or a daemon restart between preparation and start, still
/// finds it. A successor that started its own lineage adopts nothing.
pub(in crate::host) fn adopt_resumed(
    host: &SharedHost,
    successor: &chat::Session,
) -> Result<bool, String> {
    let Ok(registry) = registry(host) else {
        return Ok(false);
    };
    let agents = registry
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .lineage_agents(successor.id)?;
    let mut predecessors = agents.into_iter().filter_map(|agent| match agent.session {
        Some(HostedSession::Chat(session)) => Some(session),
        _ => None,
    });
    let Some(predecessor) = predecessors.next() else {
        return Ok(false);
    };
    if predecessors.next().is_some() {
        return Err("More than one open agent claims the resumed conversation's lineage".into());
    }
    adopt_successor(host, &predecessor, successor)
}

/// Bind a stopped agent to the process that resumed its conversation under a
/// new chat ID. `Registry::adopt_successor` states the exact contract; no
/// execution authority is issued here. Takes the agent's dispatch gate, so
/// call it without holding that gate or the host lock.
pub(in crate::host) fn adopt_successor(
    host: &SharedHost,
    predecessor: &chat::Session,
    successor: &chat::Session,
) -> Result<bool, String> {
    let Ok(registry) = registry(host) else {
        return Ok(false);
    };
    let Some(agent) = registry
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .agent_for_session(predecessor.id)?
    else {
        return Ok(false);
    };
    // Close and dispatch decide under this gate whether the agent's process
    // ended; adoption must not change that between their check and commit.
    let gate = agent_gate(host, agent.id);
    let _dispatch = gate.lock().unwrap_or_else(|p| p.into_inner());
    {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        if state
            .chats
            .get(&predecessor.id)
            .is_some_and(|record| !record.handle.is_completed())
        {
            return Err("The predecessor is still running in this daemon".into());
        }
        if successor.proof.as_ref() != Some(state.proof()) {
            return Err("The successor was not started by this daemon".into());
        }
    }
    let changed = registry
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .adopt_successor(predecessor, successor)?;
    if changed {
        announce(host, &registry);
    }
    Ok(changed)
}

/// Whether closing `agent` may rely on its bound process having been left by
/// an earlier kernel boot: an open agent with no unconfirmed launch, bound to
/// a chat whose complete proof names another boot. A scheduled agent is its
/// automation's to end, and keeps the ordinary refusal.
fn lost_with_earlier_boot(
    host: &SharedHost,
    agent: &Agent,
    pending: Option<&HostedSession>,
) -> bool {
    !agent.closed
        && pending.is_none()
        && !matches!(agent.origin, Origin::Automation { .. })
        && matches!(&agent.session, Some(HostedSession::Chat(bound))
            if super::chat::from_earlier_boot(host, bound))
}

/// Holds a lost agent's close against anything that could be running under it
/// in this boot. A startup cleanup owner retained for the agent is a launch of
/// this boot whose teardown is unconfirmed.
fn fence_lost_agent(
    host: &SharedHost,
    agent: &Agent,
    session: &chat::Session,
) -> Result<super::chat::LostHolderFence, String> {
    if host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .execution_startups
        .contains_key(&agent.id)
    {
        return Err(
            "Startup teardown remains unconfirmed; its exact cleanup owner was retained".into(),
        );
    }
    super::chat::fence_lost_holder(host, session, agent.console.as_ref())
}

/// A pane's Close of an agent an earlier boot left, which has no actor to
/// close and no recorded daemon stop. `false` leaves anything else to the
/// ordinary retained close.
pub(super) fn close_lost_agent_from_pane(
    host: &SharedHost,
    session: &chat::Session,
) -> Result<bool, String> {
    let Ok(registry) = registry(host) else {
        return Ok(false);
    };
    let bound_here = |agent: &Agent| {
        matches!(&agent.session, Some(HostedSession::Chat(bound))
            if crate::agent_execution::same_chat_process(bound, session))
    };
    let Some(agent) = registry
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .agent_for_session(session.id)?
        .filter(bound_here)
    else {
        return Ok(false);
    };
    let gate = agent_gate(host, agent.id);
    let _dispatch = gate.lock().unwrap_or_else(|p| p.into_inner());
    // Decided under the gate: a close or adoption may have just run.
    let (agent, pending) = {
        let store = registry.lock().unwrap_or_else(|p| p.into_inner());
        (
            store.agent(agent.id)?,
            store.pending_agent_launch(agent.id)?,
        )
    };
    if !bound_here(&agent) || !lost_with_earlier_boot(host, &agent, pending.as_ref()) {
        // Includes an unconfirmed launch, which the ordinary close refuses
        // before changing anything.
        return Ok(false);
    }
    let Some(HostedSession::Chat(bound)) = &agent.session else {
        return Ok(false);
    };
    let _fence = fence_lost_agent(host, &agent, bound)?;
    retired(host, bound)?;
    Ok(true)
}

/// What closing this chat does with the work its provider detached, such as
/// a server a command started with `nohup`. A conversation someone attends
/// leaves it running: they may have asked for it. An agent nobody attends, an
/// automation's or one a session started until a person takes part in it,
/// stops it, so unattended runs do not leave processes behind. Decided at the
/// close, since a person can take part after launch. Without the optional
/// agent registry no conversation is an agent; an agent that cannot be read
/// keeps the stop every close made before.
pub(super) fn detached_work_on_close(
    host: &SharedHost,
    session: TerminalId,
) -> crate::daemon::owned_children::DetachedWork {
    use crate::daemon::owned_children::DetachedWork;
    let Ok(registry) = registry(host) else {
        return DetachedWork::Spare;
    };
    let registry = registry.lock().unwrap_or_else(|p| p.into_inner());
    let unattended = registry.agent_for_session(session).and_then(|agent| {
        agent.map_or(Ok(false), |agent| {
            if matches!(agent.origin, Origin::User) {
                Ok(false)
            } else {
                registry
                    .agent_is_interactive(agent.id)
                    .map(|attended| !attended)
            }
        })
    });
    match unattended {
        Ok(false) => DetachedWork::Spare,
        Ok(true) => DetachedWork::Stop,
        Err(error) => {
            tracing::warn!(%error, "closing a chat stops its detached work: its agent is unknown");
            DetachedWork::Stop
        }
    }
}

/// Reads only the managed presentation. No registry means ordinary interactive
/// conversations still close without the optional managed-agent subsystem.
pub(super) fn preflight_retirement(
    host: &SharedHost,
    session: &chat::Session,
) -> Result<(), String> {
    let Ok(registry) = registry(host) else {
        return Ok(());
    };
    let result = registry
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .preflight_agent_retirement(&HostedSession::Chat(session.clone()));
    result
}

/// Serializes an agent's operations. The value is the run whose dispatcher
/// released the gate to await provider readiness: until that dispatcher
/// takes it back, the provider has not received the run's prompt.
fn agent_gate(host: &SharedHost, id: AgentId) -> Arc<Mutex<Option<Undelivered>>> {
    let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
    // A holder or waiter keeps its Arc alive, so pruning cannot split an
    // active agent's serialization. Failed and repeated closes need no
    // special cleanup, and unused entries survive only until the next lookup.
    state
        .execution_gates
        .retain(|_, gate| gate.strong_count() > 0);
    if let Some(gate) = state
        .execution_gates
        .get(&id)
        .and_then(std::sync::Weak::upgrade)
    {
        return gate;
    }
    let gate = Arc::new(Mutex::new(None));
    state.execution_gates.insert(id, Arc::downgrade(&gate));
    gate
}

fn close(host: &SharedHost, authority: &Authority, id: AgentId) -> Result<Agent, String> {
    let origin = origin(host, authority)?;
    let gate = agent_gate(host, id);
    let _dispatch = gate.lock().unwrap_or_else(|p| p.into_inner());
    let registry = registry(host)?;
    let agent = registry
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .preflight_agent_close(&origin, id)?;
    if agent.closed {
        // Older daemons closed the worker without retiring its shared pane.
        // Repeating close repairs only that retained presentation; it does not
        // start or contact a provider, and keeps the completed run history.
        let agent = registry
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .close_agent(&origin, id)?;
        announce(host, &registry);
        return Ok(agent);
    }
    let (pending, ended) = {
        let store = registry.lock().unwrap_or_else(|p| p.into_inner());
        (
            store.pending_agent_launch(id)?,
            store.agent_process_ended(id)?,
        )
    };
    // A daemon stop already proved this agent's process ended and revoked its
    // authority; only its saved conversation remains to retire.
    let mut lost_holder_fence = None;
    if let Some(source) = agent
        .session
        .as_ref()
        .or(pending.as_ref())
        .filter(|_| !ended)
    {
        match source {
            HostedSession::Chat(session) => {
                let hosted = {
                    let state = host.lock().unwrap_or_else(|p| p.into_inner());
                    state
                        .chats
                        .get(&session.id)
                        .map(|record| {
                            super::chat::CloseTarget::for_execution(&state, record, session)
                        })
                        .transpose()
                        .map_err(|error| error.message)?
                };
                if let Some(target) = hosted {
                    super::chat::close_captured(host, target).map_err(|error| error.message)?;
                } else if lost_with_earlier_boot(host, &agent, pending.as_ref()) {
                    // A changed kernel boot proves that process is gone, even
                    // with no daemon stop recorded for it. The fence refuses
                    // anything started since, and holds until the close
                    // commits. An unconfirmed launch has no such process to
                    // reason about, and stays with the refusal below.
                    lost_holder_fence = Some(fence_lost_agent(host, &agent, session)?);
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
                    } else if session.closes_without_hosted_actor()
                        || matches!(
                            session.native_continuity,
                            chat::NativeContinuity::RetainedClaudeActor { .. }
                        )
                    {
                        // Missing history can prevent actor restoration while
                        // its authenticated provider holder is still alive.
                        // Only the holder's exact receipt releases ownership,
                        // or, for an owner that died without one, proof that
                        // nothing of it can still run.
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
            .retire_agent_process_authority(source, lost_holder_fence.is_some())?;
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
    drop(lost_holder_fence);
    host.lock()
        .unwrap_or_else(|p| p.into_inner())
        .execution_startups
        .remove(&id);
    announce(host, &registry);
    Ok(result)
}

/// Preserve typed mailbox backpressure at the service boundary. Existing
/// operations retain their original validation and String-returning API.
pub(super) fn process_request(
    host: &SharedHost,
    authority: Authority,
    operation: crate::agent_execution::Operation,
) -> Result<crate::agent_execution::Reply, crate::agent_execution::AcceptanceError> {
    match operation {
        crate::agent_execution::Operation::SendMessage(outgoing) => {
            mailbox::send(host, &authority, outgoing).map(crate::agent_execution::Reply::Sent)
        }
        crate::agent_execution::Operation::Send(outgoing) => {
            mailbox::post(host, &authority, outgoing).map(crate::agent_execution::Reply::Delivered)
        }
        other => process(host, authority, other).map_err(Into::into),
    }
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
        Operation::Directory(scope) => {
            directory::read(host, &authority, scope).map(Reply::Directory)
        }
        Operation::AccountInfo(query) => {
            account_info::read(host, &authority, query).map(Reply::AccountInfo)
        }
        Operation::Requests(id) => {
            let (_, _, handle) = owned_provider(host, &authority, id)?;
            refresh_retained_requests(&handle);
            Ok(Reply::Requests(displayed_requests(&handle)))
        }
        Operation::Control { agent, command } => {
            control(host, &authority, agent, command).map(Reply::Requests)
        }
        Operation::SendMessage(outgoing) => mailbox::send(host, &authority, outgoing)
            .map(Reply::Sent)
            .map_err(|error| error.to_string()),
        Operation::Inbox(query) => mailbox::inbox(host, &authority, query).map(Reply::Inbox),
        Operation::PeekInbox(query) => mailbox::peek(host, &authority, query).map(Reply::Inbox),
        Operation::AcknowledgeInbox(receipt) => {
            mailbox::acknowledge(host, &authority, receipt).map(|()| Reply::InboxAcknowledged)
        }
        Operation::Transcript(query) => {
            transcript::read(host, &authority, query).map(Reply::Transcript)
        }
        Operation::Members(channel) => {
            directory::channel(host, &authority, channel).map(Reply::Members)
        }
        Operation::Sessions(scope) => {
            directory::sessions(host, &authority, scope).map(Reply::Sessions)
        }
        Operation::Send(outgoing) => mailbox::post(host, &authority, outgoing)
            .map(Reply::Delivered)
            .map_err(|error| error.to_string()),
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

/// A retained provider reports its run Waiting through the durable outbox,
/// which can overtake the proxy's cached metadata, so a caller reading a
/// Waiting run's requests refreshes that metadata first. A failed refresh
/// leaves the cached requests, which the actor's own pump keeps converging.
fn refresh_retained_requests(handle: &chat::runtime::Handle) {
    let _ = handle.refresh_retained_state();
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
///
/// A run settles when its main turn ends, while a background subagent may
/// still wait on a decision. The owner may still turn that down, since a
/// refusal authorizes no work, but approving it needs an active run.
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
    let mut dispatch = gate.lock().unwrap_or_else(|p| p.into_inner());
    let (_, run, handle) = owned_provider(host, authority, id)?;
    if registry(host)?
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .agent_is_interactive(id)?
    {
        return Err("A person took this agent over; its controls are theirs".into());
    }
    if run.state.is_terminal() && !matches!(&command, Control::Answer { .. }) {
        return Err(format!(
            "The agent has no active run; its latest is {:?}",
            run.state
        ));
    }
    let answered = match command {
        Control::Interrupt {
            including_subagents,
        } => {
            if !withdraw_undelivered(host, &mut dispatch, Some(run.id))? {
                handle.interrupt_owned(Some(run.id), including_subagents)?;
            }
            None
        }
        Control::Answer {
            request_id,
            action,
            answers,
        } => {
            refresh_retained_requests(&handle);
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
            let command = Command::Answer {
                request_id: request_id.clone(),
                action,
                answers,
            };
            let synchronization = super::chat::retained_actor::AnswerSync::capture(host, &handle)?;
            if run.state.is_terminal() {
                handle.decline_answer(command)?;
            } else {
                handle.command(command)?;
            }
            if let Some(synchronization) = synchronization {
                synchronization.complete(host).map_err(|error| format!(
                    "The answer was accepted, but its durable run state is unconfirmed: {error}. Do not repeat the answer."
                ))?;
            }
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
            let mut dispatch = gate.lock().unwrap_or_else(|p| p.into_inner());
            if !withdraw_undelivered(host, &mut dispatch, None)? {
                handle.interrupt_owned(None, including_subagents)?;
            }
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
            let (text, truncated) = console_text(&snapshot, chat::now_ms().unwrap_or(0));
            let mut requests = snapshot.requests;
            hide_secrets(&mut requests);
            Ok(Reply::Console(Some(ConsoleView {
                generation: snapshot.generation,
                text,
                truncated,
                requests,
                ready: snapshot.ready,
                run,
            })))
        }
    }
}

/// How long a step runs before the console says so. Most finish sooner, and
/// their one line is enough; a long one would otherwise look like a hang.
const CONSOLE_RUNNING_AFTER_MILLIS: u64 = 5_000;
/// The longest line a provider notice prints in the console.
const CONSOLE_NOTICE_BYTES: usize = 200;

/// The newest console lines that fit the reply, and whether any were left out.
/// Items are kept or dropped whole, so a line the client printed never comes
/// back shorter. Only an item too long to fit at all is cut.
///
/// Every line ends in a newline except the one the provider is still writing:
/// the client holds that back until it is complete.
fn console_text(snapshot: &chat::Snapshot, now: u64) -> (String, bool) {
    let mut chunks = Vec::new();
    let mut remaining = crate::agent_execution::MAX_RESULT_BYTES;
    let mut truncated = snapshot.has_earlier;
    let paged_files = console_paged_files(snapshot);
    for item in snapshot.items.iter().rev() {
        let line = match paged_files.get(item.id.as_str()) {
            Some(0) => None,
            Some(1) => Some("Edited 1 more file\n".to_owned()),
            Some(files) => Some(format!("Edited {files} more files\n")),
            None => console_line(item, now),
        };
        let Some(mut text) = line else {
            continue;
        };
        if text.len() > remaining {
            truncated = true;
            if chunks.is_empty() {
                crate::agent_execution::truncate(&mut text, remaining);
                text.pop();
                text.push('\n');
                chunks.push(text);
            }
            break;
        }
        remaining -= text.len();
        chunks.push(text);
    }
    chunks.reverse();
    let mut text = chunks.concat();
    // A provider opens its next text block empty, which has no line to hold.
    let writing = snapshot.busy
        && snapshot.items.last().is_some_and(|item| {
            matches!(
                item.content,
                chat::ItemContent::Text {
                    thinking: false,
                    ..
                }
            ) && console_line(item, now).is_some()
        });
    if writing {
        text.pop();
    }
    (text, truncated)
}

/// How many files each later part of a paged edit adds to those its earlier
/// parts named. A part names the files it has lines of, so a file can span parts.
fn console_paged_files(snapshot: &chat::Snapshot) -> std::collections::HashMap<&str, usize> {
    let mut named = std::collections::HashSet::new();
    let mut files = std::collections::HashMap::new();
    for item in &snapshot.items {
        let chat::ItemContent::Changes { changes, status } = &item.content else {
            continue;
        };
        let edit = chat::TranscriptPart::parse(&item.id).map_or(item.id.as_str(), |part| part.item);
        let added = changes
            .iter()
            .filter(|change| named.insert((edit, change.path.as_str())))
            .count();
        if item.continues_paged_item() && *status != chat::ToolStatus::Running {
            files.insert(item.id.as_str(), added);
        }
    }
    files
}

/// What the provider's harness said about the session, as one unlabelled line,
/// the way the native chat recesses it. Adapters file command output and whole
/// protocol frames under this role, so only the start of plain text is shown.
fn console_notice(text: &str) -> Option<String> {
    let text = text.trim();
    if serde_json::from_str::<serde_json::Value>(text)
        .is_ok_and(|value| value.is_object() || value.is_array())
    {
        return None;
    }
    let mut line = text.lines().next()?.trim_end().to_owned();
    if line.len() > CONSOLE_NOTICE_BYTES {
        crate::agent_execution::truncate(&mut line, CONSOLE_NOTICE_BYTES);
    }
    line.push('\n');
    Some(line)
}

/// One console line per transcript item. The console is a plain-text view a
/// person reads, so it shows what the agent said, what it was sent, and a
/// one-line account of each step, named as the native chat view names it.
/// Command output, tool payloads and delivered reports are left to that view,
/// which discloses them on demand.
///
/// A step appears once it settles, or as running once it has taken a while, so
/// finishing it adds a line instead of rewriting one the client already printed.
/// A step that settles after later text still lands before that text; the client
/// then prints only the lines it has not shown (`ConsoleTranscript` in the CLI).
fn console_line(item: &chat::Item, now: u64) -> Option<String> {
    use chat::{ChangeKind, CommandAction, DeliveryKind, ItemContent, ToolStatus};
    let running = item.content.status() == Some(ToolStatus::Running);
    if running
        && !item
            .started_at
            .is_some_and(|started| now.saturating_sub(started) >= CONSOLE_RUNNING_AFTER_MILLIS)
    {
        return None;
    }
    if item.continues_paged_item() {
        // A notice prints one line, which its first part already gave.
        if item.role == chat::Role::System {
            return None;
        }
        return match &item.content {
            ItemContent::Text {
                text,
                thinking: false,
                ..
            }
            | ItemContent::Delivery {
                kind: DeliveryKind::Message,
                body: text,
                ..
            } if !text.trim().is_empty() => Some(format!("{}\n", text.trim_end())),
            _ => None,
        };
    }
    let outcome = |status: &ToolStatus| match status {
        ToolStatus::Succeeded | ToolStatus::Running => "",
        ToolStatus::Failed => " (failed)",
        ToolStatus::Denied => " (denied)",
        ToolStatus::Stopped => " (stopped)",
        ToolStatus::Waiting => " (waiting on its own background work)",
    };
    let first_line = |text: &str| text.trim().lines().next().unwrap_or_default().to_owned();
    let line = match &item.content {
        ItemContent::Text {
            text,
            thinking: false,
            ..
        } if !text.trim().is_empty() => {
            let text = text.trim_end();
            return match item.role {
                chat::Role::User => Some(format!("User: {text}\n")),
                chat::Role::Assistant => Some(format!("Assistant: {text}\n")),
                chat::Role::System => console_notice(text),
            };
        }
        ItemContent::Text { .. } => return None,
        ItemContent::Command {
            label,
            command,
            action,
            status,
            exit_code,
            ..
        } => {
            let (verb, doing, stored) = match action {
                CommandAction::Run => ("Ran", "Running", None),
                CommandAction::Read => ("Read", "Reading", Some("Read ")),
                CommandAction::Search => ("Searched", "Searching", Some("Search ")),
                CommandAction::List => ("Listed", "Listing", Some("List ")),
            };
            let verb = if running { doing } else { verb };
            let label = chat::without_page_mark(label.trim());
            // Transcripts stored before adapters dropped the verb from the
            // label still carry it, as the native view also allows for.
            let label = stored
                .and_then(|verb| label.strip_prefix(verb))
                .filter(|target| !target.is_empty())
                .unwrap_or(label);
            let target = first_line(if label.is_empty() { command } else { label });
            let exit = match (status, exit_code) {
                (ToolStatus::Failed, Some(code)) => format!(" (failed, exit {code})"),
                _ => outcome(status).to_owned(),
            };
            format!("{verb} {target}").trim_end().to_owned() + &exit
        }
        ItemContent::Changes { changes, status } => match changes.as_slice() {
            [] => return None,
            [single] => {
                let verb = match (&single.kind, running) {
                    (ChangeKind::Added, false) => "Added",
                    (ChangeKind::Deleted, false) => "Deleted",
                    (ChangeKind::Updated, false) => "Edited",
                    (ChangeKind::Renamed { .. }, false) => "Renamed",
                    (ChangeKind::Added, true) => "Adding",
                    (ChangeKind::Deleted, true) => "Deleting",
                    (ChangeKind::Updated, true) => "Editing",
                    (ChangeKind::Renamed { .. }, true) => "Renaming",
                };
                format!("{verb} {}{}", single.path, outcome(status))
            }
            many if running => format!("Editing {} files", many.len()),
            many => format!("Edited {} files{}", many.len(), outcome(status)),
        },
        ItemContent::WebSearch { query, .. } if query.trim().is_empty() => return None,
        ItemContent::WebSearch { query, .. } if running => {
            format!("Searching the web for {}", first_line(query))
        }
        ItemContent::WebSearch { query, .. } => {
            format!("Searched the web for {}", first_line(query))
        }
        ItemContent::Tool { name, .. } if running => {
            format!("Calling {}", chat::without_page_mark(name))
        }
        ItemContent::Tool { name, status, .. } => {
            format!(
                "Called {}{}",
                chat::without_page_mark(name),
                outcome(status)
            )
        }
        ItemContent::Image { description, .. } if description.trim().is_empty() => {
            "Viewed an image".to_owned()
        }
        ItemContent::Image { description, .. } => {
            format!("Viewed {}", first_line(description))
        }
        ItemContent::Delivery {
            kind,
            from,
            sender,
            summary,
            body,
        } => {
            let heading = chat::delivery_heading(*kind, from, sender, summary);
            // A message from another session is what the agent is asked to
            // act on, so it stays readable. Reports and notices are payload.
            match kind {
                DeliveryKind::Message if !body.trim().is_empty() => {
                    format!("{heading}:\n{}", body.trim_end())
                }
                _ => heading,
            }
        }
    };
    Some(line + "\n")
}

#[cfg(test)]
pub(super) mod tests;
