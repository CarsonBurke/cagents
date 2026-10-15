//! Recreate a paired view without replacing the accepted agent or its run.
use super::*;
use crate::model::TerminalSession;

#[derive(Clone)]
pub(in crate::host) struct PairPlan {
    source: chat::Session,
    registry: Option<SharedRegistry>,
    agent: Option<Agent>,
    kind: Kind,
}
#[derive(Clone)]
enum Kind { None, Execution, Codex(crate::model::LaunchRecipe) }

pub(in crate::host) fn prepare_restart_pair(host: &SharedHost, source: &chat::Session) -> Result<PairPlan, String> {
    let registry = host.lock().unwrap_or_else(|p| p.into_inner()).execution_registry.clone();
    let agent = registry.as_ref().map(|registry| registry.lock().unwrap_or_else(|p|p.into_inner()).agent_for_session(source.id)).transpose()?.flatten();
    if let Some(agent) = &agent {
        let expected = agent.session.as_ref().ok_or("The accepted agent lost its exact conversation binding")?;
        if !expected.same_process(&HostedSession::Chat(source.clone())) {
            return Err("The accepted agent belongs to a different conversation owner".into());
        }
        if agent.console.as_ref().is_some_and(|console| source.terminal.as_ref() != Some(console)) {
            return Err("The accepted console does not match the stopped provider pair; no owner was stopped".into());
        }
        if let HostedSession::Chat(saved) = expected {
            if saved.launch.session_id.as_ref().is_some_and(|id| source.launch.session_id.as_ref() != Some(id))
                || saved.owned_codex_listener != source.owned_codex_listener
            { return Err("The accepted agent changed its exact thread or listener before restart".into()); }
        }
    }
    let kind = match &source.terminal {
        None => Kind::None,
        Some(terminal) => {
            if terminal.id != source.id || terminal.continuity.live_proof().as_ref() != source.proof.as_ref() {
                return Err("The stopped provider has an inconsistent paired view".into());
            }
            if let Some(id) = crate::agent_execution::console_agent(&terminal.launch) {
                let expected = agent.as_ref().filter(|agent| agent.id == id && agent.console.as_ref() == Some(terminal))
                    .ok_or("The paired console has no exact accepted agent binding; recover its original owner before restarting")?;
                if expected.closed || terminal.launch.working_directory != source.launch.recipe.working_directory {
                    return Err("The paired console no longer belongs to this accepted agent".into());
                }
                // A console is an exact matched, thin control view. It cannot
                // carry a shell command, provider prompt or account overrides.
                let matched = crate::companion::Product::Cli.beside_current().map_err(|e|e.to_string())?;
                if std::path::Path::new(&terminal.launch.program) != matched.as_path()
                    || !terminal.launch.environment.is_empty() || !terminal.launch.unset_environment.is_empty()
                { return Err("The paired console launch is not the exact matched control view".into()); }
                Kind::Execution
            } else if source.launch.provider == crate::agent::Provider::Codex && source.owns_codex_listener() {
                Kind::Codex(source.launch.terminal_recipe().ok_or("The paired Codex view has no exact confirmed resume identity")?)
            } else {
                return Err("This paired terminal has no safe automatic restart view. Reconnect its original terminal before restarting the provider".into());
            }
        }
    };
    Ok(PairPlan { source: source.clone(), registry, agent, kind })
}

fn current(host: &SharedHost, id: TerminalId) -> Result<(chat::Session, chat::runtime::Handle), String> {
    let state = host.lock().unwrap_or_else(|p|p.into_inner());
    let record = state.chats.get(&id).filter(|record| !record.handle.is_completed())
        .ok_or("The restarted provider is not available; its accepted binding was retained")?;
    let mut session = record.session.clone();
    session.launch.session_id = record.handle.confirmed_session_id();
    session.native_continuity = record.handle.native_continuity();
    Ok((session, record.handle.clone()))
}

pub(in crate::host) fn restore_restart_pair(host: &SharedHost, client: super::super::state::ClientId, plan: &PairPlan) -> Result<chat::RestartReceipt, String> {
    let (mut session, handle) = current(host, plan.source.id)?;
    if session.proof != plan.source.proof || session.launch.provider != plan.source.launch.provider
        || session.launch.recipe != plan.source.launch.recipe
        || session.launch.session_id != plan.source.launch.session_id
        || session.owned_codex_listener != plan.source.owned_codex_listener
    { return Err("The restarted provider changed its immutable conversation configuration".into()); }
    // Reconnect-only providers can leave their exact console running. Rebind
    // that existing worker instead of opening a second view.
    let live_pair = if matches!(plan.kind, Kind::Execution) {
        let state = host.lock().unwrap_or_else(|p|p.into_inner());
        state.terminals.get(&session.id).filter(|terminal| !terminal.handle.is_completed())
            .map(|terminal| (terminal.session.clone(),terminal.handle.clone()))
    } else { None };
    if let Some((terminal, worker)) = live_pair {
        if plan.source.terminal.as_ref() != Some(&terminal) {
            return Err("The console was replaced while its provider restarted; the replacement was not touched".into());
        }
        publish_pair(host, &session, &handle, &terminal, &worker)?;
    } else if !matches!(plan.kind, Kind::None) {
        // Durable restart already proved the old pair closed. Never overwrite
        // another live worker merely because it reused the public terminal ID.
        if host.lock().unwrap_or_else(|p|p.into_inner()).terminals.contains_key(&session.id) {
            return Err("The stopped console still has an unresolved worker; no second console was opened".into());
        }
        let mut retained = session.clone();
        retained.terminal = None;
        chat::protocol::align_resume_identity(&mut retained.launch)?;
        handle.update_native_source(retained, None, None)?;
        {
            let mut state = host.lock().unwrap_or_else(|p|p.into_inner());
            let record = state.chats.get_mut(&session.id).filter(|record| record.handle.same_actor(&handle))
                .ok_or("The provider changed while releasing its stopped console")?;
            record.session.terminal = None;
            record.paired_terminal = None;
            record.console_recipe = None;
        }
        session.terminal = None;
        match &plan.kind {
            Kind::Execution => {
                let registry = plan.registry.as_ref().ok_or("The accepted agent registry is unavailable")?;
                let agent = plan.agent.as_ref().ok_or("The accepted console owner is unavailable")?;
                return create_console_with_restart(host, registry, agent.id, &session, &handle, Some((agent,&plan.source)))
                    .map_err(|error| match error {ConsoleError::Known(error)|ConsoleError::Uncertain(error)=>error})?
                    .ok_or("The paired console restart did not return its exact receipt".into());
            }
            Kind::Codex(recipe) => {
                super::super::chat::codex_terminal_view(host,client,session.id,&handle,session.clone(),recipe.clone())
                    .map_err(|error|error.message)?;
            }
            Kind::None => {}
        }
    }
    let (replacement, actual) = current(host, session.id)?;
    if !actual.same_actor(&handle) { return Err("The provider changed before its pair receipt was published".into()); }
    let receipt = chat::RestartReceipt::confirm(plan.source.clone(), replacement)?;
    if let (Some(registry),Some(agent)) = (&plan.registry,&plan.agent) {
        registry.lock().unwrap_or_else(|p|p.into_inner()).rebind_restarted_agent(agent,&receipt)?;
        announce(host,registry);
    }
    Ok(receipt)
}

fn publish_pair(host:&SharedHost, session:&chat::Session, handle:&chat::runtime::Handle, terminal:&TerminalSession, worker:&crate::terminal::TerminalHandle) -> Result<(),String> {
    let mut retained = session.clone();
    retained.terminal = Some(terminal.clone());
    chat::protocol::align_resume_identity(&mut retained.launch)?;
    handle.update_native_source(retained,Some(terminal.launch.clone()),worker.holder_identity().cloned())?;
    let mut state=host.lock().unwrap_or_else(|p|p.into_inner());
    if state.terminals.get(&terminal.id).is_none_or(|record|!record.handle.same_worker(worker)) {
        return Err("The exact console worker changed before pairing".into());
    }
    let record=state.chats.get_mut(&session.id).filter(|record|record.handle.same_actor(handle))
        .ok_or("The provider changed before pairing")?;
    record.session.terminal=Some(terminal.clone());
    record.paired_terminal=Some(worker.clone());
    record.console_recipe=Some(terminal.launch.clone());
    Ok(())
}
