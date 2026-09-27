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
    model::{AgentId, LaunchRecipe, RunId, TerminalId},
    terminal_ipc::{ClientRequest, ServerResponse},
};
use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
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
            HostedSession::Terminal(source) => state
                .terminals
                .get(&source.id)
                .is_some_and(|record| !record.handle.is_completed() && record.session == source),
        };
        if !live {
            return Err("The issuing agent session is no longer live or proved adopted".into());
        }
    }
    Ok(origin)
}

fn selected_account(spec: &Spec) -> Result<Account, String> {
    let store = Store::installation().ok_or("Account registry is unavailable")?;
    let accounts = store.load().map_err(|e| e.to_string())?;
    if accounts.is_newer() {
        return Err("Account registry needs a newer cagents version".into());
    }
    let homes = Homes::installation();
    match &spec.account {
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
    mut request: Submit,
) -> Result<Accepted, String> {
    request.validate()?;
    let origin = origin(host, authority)?;
    let selected = if let Destination::New(spec) = &mut request.destination {
        spec.target.repository =
            crate::workspace_model::canonical_repository(&spec.target.repository)?;
        spec.target.directory =
            std::fs::canonicalize(&spec.target.directory).map_err(|e| e.to_string())?;
        if !spec.target.directory.is_dir() {
            return Err("The agent working directory is not a directory".into());
        }
        let mut selected = spec.clone();
        selected.account = Some(selected_account(spec)?.id());
        Some(selected)
    } else {
        None
    };
    let registry = registry(host)?;
    let accepted = registry
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .submit_agent_with_spec(&origin, &request, selected.as_ref(), now())?;
    dispatch_accepted(host, accepted.clone())?;
    Ok(accepted)
}

/// Accepts only an exact committed record. The once-only dispatch claim itself
/// commits before the worker can spawn a process or submit a provider prompt.
/// Schedulers call this after committing their atomic firing/acceptance row.
pub(super) fn dispatch_accepted(host: &SharedHost, accepted: Accepted) -> Result<(), String> {
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
            if let Err((state, diagnostic)) = dispatch(&host, &accepted) {
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

fn dispatch(host: &SharedHost, accepted: &Accepted) -> Result<(), (State, String)> {
    let before = |message| (State::Failed, message);
    let registry = registry(host).map_err(before)?;
    let request = registry
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .agent_request(accepted.run.id)
        .map_err(before)?;
    if accepted.agent.spec.surface != Surface::Chat {
        return Err(before(
            "Structured terminal execution is not available in this build".into(),
        ));
    }
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
        let mut recipe = LaunchRecipe {
            program: accepted.agent.spec.provider.executable().into(),
            arguments: Vec::new(),
            working_directory: accepted.agent.spec.target.directory.clone(),
            environment: Default::default(),
            unset_environment: account
                .credential_overrides()
                .iter()
                .map(|name| (*name).to_owned())
                .collect(),
        };
        if let Some((name, path)) = account.probe_environment() {
            recipe.environment.insert(
                name.into(),
                path.to_str()
                    .ok_or_else(|| before("Account path is not UTF-8".into()))?
                    .into(),
            );
        }
        let session = chat::Session {
            id: TerminalId::new(),
            launch: chat::Launch {
                provider: accepted.agent.spec.provider,
                recipe,
                session_id: None,
            },
            proof: None,
            terminal: None,
            owned_codex_listener: None,
        };
        let response = super::chat::process(
            host,
            EXECUTION_CLIENT,
            ClientRequest::ChatStart {
                session,
                source: None,
                terminate_source: false,
                execution_environment: Default::default(),
            },
        )
        .map_err(|e| before(e.message))?;
        let ServerResponse::ChatAttached { session, .. } = response else {
            return Err(before(
                "Provider start returned an unexpected response".into(),
            ));
        };
        let handle = host
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .chats
            .get(&session.id)
            .ok_or_else(|| before("The new agent closed before attachment".into()))?
            .handle
            .clone();
        if let Err(error) = registry
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .bind_agent(
                accepted.agent.id,
                None,
                HostedSession::Chat(session.clone()),
            )
        {
            let _ = handle.close();
            return Err(before(error));
        }
        (session, handle)
    };
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let snapshot = handle.snapshot(None).expect("unconditional snapshot");
        if snapshot.exited || handle.is_completed() {
            return Err(before("Provider exited before accepting the prompt".into()));
        }
        if snapshot.ready {
            break;
        }
        if Instant::now() >= deadline {
            return Err(before(
                "Provider did not become ready before the launch deadline".into(),
            ));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    if let Some(model) = &accepted.agent.spec.model {
        handle
            .command(Command::SetModel {
                model: model.clone(),
                reasoning: None,
            })
            .map_err(before)?;
    }
    if let Some(mode) = &accepted.agent.spec.permission_mode {
        handle
            .command(Command::SetPermissionMode { mode: mode.clone() })
            .map_err(before)?;
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

pub(super) fn observed(host: &SharedHost, session: TerminalId, run: RunId, update: Update) {
    let Ok(registry) = registry(host) else { return };
    let mut registry = registry.lock().unwrap_or_else(|p| p.into_inner());
    let result = (|| {
        let current = registry.agent_run(run)?;
        let agent = registry.agent(current.agent)?;
        if agent.closed
            || agent.session.as_ref().map(HostedSession::id) != Some(session)
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
}

fn finish_failure(host: &SharedHost, run: RunId, state: State, diagnostic: String) {
    if let Ok(registry) = registry(host) {
        finish_failure_registry(&registry, run, state, diagnostic);
    }
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
