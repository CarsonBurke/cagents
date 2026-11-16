//! Serialize automatic cleanup with interactive ownership changes. Displaying
//! a conversation is not takeover; issuing work or opening native controls is.
use super::*;
use crate::terminal_ipc::{ProtocolError, ProtocolErrorCode};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::host) enum Reconciliation {
    Closed,
    Idle,
    /// The exact owned provider is busy or changed before idle close; retain its slot.
    Busy,
    UserOwned,
}

pub(in crate::host) fn interactive(
    host: &SharedHost,
    client: super::super::state::ClientId,
    request: ClientRequest,
    apply: impl FnOnce(ClientRequest) -> Result<ServerResponse, ProtocolError>,
) -> Result<ServerResponse, ProtocolError> {
    if client == EXECUTION_CLIENT {
        return apply(request);
    }
    let (id, claims) = match &request {
        ClientRequest::ChatCommand { chat_id, command } => (
            *chat_id,
            !matches!(command, Command::Refresh | Command::LoadEarlier),
        ),
        ClientRequest::ChatReturnTerminal { chat_id } => (*chat_id, true),
        ClientRequest::ChatCommitTerminal { terminal_id } => (*terminal_id, false),
        ClientRequest::ChatClose { chat_id } => (*chat_id, false),
        _ => return apply(request),
    };
    let registry = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .execution_registry
        .clone();
    let Some(registry) = registry else {
        return apply(request);
    };
    let unavailable =
        |message: String| ProtocolError::new(ProtocolErrorCode::TerminalUnavailable, message, true);
    let agent = registry
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .agent_for_session(id)
        .map_err(unavailable)?;
    let Some(agent) = agent else {
        return apply(request);
    };
    let gate = agent_gate(host, agent.id);
    let mut dispatch = gate.lock().unwrap_or_else(|p| p.into_inner());
    if claims {
        let authorized = host
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .chats
            .get(&id)
            .is_some_and(|record| {
                record.subscribers.contains_key(&client)
                    && agent.session.as_ref().is_some_and(|source| {
                        source.same_process(&HostedSession::Chat(record.session.clone()))
                    })
            });
        if !authorized {
            return Err(unavailable(
                "The exact agent session is no longer attached".into(),
            ));
        }
        registry
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .claim_agent_interaction(agent.id, None)
            .map_err(unavailable)?;
        // Stopping a run whose provider is still starting withdraws its prompt;
        // otherwise its dispatcher would deliver it once the provider is ready.
        // The provider has no turn yet, so the interrupt stops there.
        if let ClientRequest::ChatCommand {
            command: Command::Interrupt,
            ..
        } = &request
            && withdraw_undelivered(host, &mut dispatch, None).map_err(unavailable)?
        {
            return Ok(ServerResponse::Accepted);
        }
    }
    apply(request)
}

/// Reconcile only the exact finished firing inspected by the scheduler. A
/// later run or interactive ownership change invalidates that receipt. A close
/// is reported only after normal synchronous teardown (or its durable receipt).
pub(in crate::host) fn reconcile_agent_run(
    host: &SharedHost,
    expected: &Run,
    retire: bool,
) -> Result<Reconciliation, String> {
    let control = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .control_gate
        .clone();
    let _control = control.read().unwrap_or_else(|p| p.into_inner());
    reconcile_agent_run_under_control(host, expected, retire)
}

/// Management RPC variant; its caller already holds the daemon control gate.
pub(in crate::host) fn reconcile_agent_run_under_control(
    host: &SharedHost,
    expected: &Run,
    retire: bool,
) -> Result<Reconciliation, String> {
    if !expected.state.is_terminal() || expected.state == State::Interrupted {
        return Err("An unfinished or uncertain run cannot be automatically reconciled".into());
    }
    super::super::client::ensure_control_open(host).map_err(|error| error.message)?;
    let gate = agent_gate(host, expected.agent);
    let dispatch = gate.lock().unwrap_or_else(|p| p.into_inner());
    let registry = registry(host)?;
    let agent = {
        let registry = registry.lock().unwrap_or_else(|p| p.into_inner());
        let agent = registry.agent(expected.agent)?;
        if !matches!(agent.origin, Origin::Automation { .. }) {
            return Err("Automatic cleanup requires an automation-owned agent".into());
        }
        if registry.agent_is_interactive(agent.id)? && !agent.closed {
            return Ok(Reconciliation::UserOwned);
        }
        if registry.latest_agent_run(agent.id)? != *expected {
            return Err("A newer run changed this agent before automatic reconciliation".into());
        }
        if agent.closed {
            return Ok(Reconciliation::Closed);
        }
        // The run was withdrawn while its provider started. Its dispatcher still
        // owns that startup and wakes the scheduler once it lets go.
        if dispatch.is_some_and(|claim| claim.run == expected.id) {
            return Ok(Reconciliation::Busy);
        }
        if registry.pending_agent_launch(agent.id)?.is_some() {
            return Err("Provider startup ownership remains unconfirmed".into());
        }
        agent
    };
    if let Some(source) = &agent.session {
        let HostedSession::Chat(source) = source else {
            return Err("This agent has no native cleanup contract".into());
        };
        let (handle, previous_boot) = {
            let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
            let record = state.chats.get(&source.id).filter(|record| {
                HostedSession::Chat(source.clone())
                    .same_process(&HostedSession::Chat(record.session.clone()))
            });
            // A provider from an earlier kernel boot cannot still own a
            // process. Retire only its completed historical execution; never
            // borrow cleanup authority from a replacement with the same ID.
            let previous_boot = retire
                && state.proof().is_complete()
                && source.proof.as_ref().is_some_and(|proof| {
                    proof.is_complete() && proof.boot_id != state.proof().boot_id
                })
                && !state.chats.contains_key(&source.id)
                && !state.terminals.contains_key(&source.id)
                && !state.any_live(&[source.id])
                && source
                    .terminal
                    .as_ref()
                    .is_none_or(|terminal| !state.any_live(&[terminal.id]))
                && agent
                    .console
                    .as_ref()
                    .is_none_or(|terminal| !state.any_live(&[terminal.id]))
                && !state.chat_spawning.contains_key(&source.id)
                && !state.chat_handoffs.contains_key(&source.id)
                && !state.native_closing.contains_key(&source.id)
                && !state.native_actor_startups.contains_key(&source.id);
            let handle = record.map(|record| record.handle.clone());
            if previous_boot {
                state.chat_spawning.insert(source.id, source.clone());
                state
                    .historical_chat_retirements
                    .insert(source.id, source.clone());
                state.inventory_generation += 1;
            }
            (handle, previous_boot)
        };
        if previous_boot {
            struct HistoricalRetirement<'a>(&'a SharedHost, &'a chat::Session);
            impl Drop for HistoricalRetirement<'_> {
                fn drop(&mut self) {
                    let mut state = self.0.lock().unwrap_or_else(|p| p.into_inner());
                    if state.historical_chat_retirements.get(&self.1.id) == Some(self.1) {
                        state.historical_chat_retirements.remove(&self.1.id);
                    }
                    if state.chat_spawning.get(&self.1.id) == Some(self.1) {
                        state.chat_spawning.remove(&self.1.id);
                        state.inventory_generation += 1;
                    }
                }
            }
            let _reservation = HistoricalRetirement(host, source);
            let mut store = registry.lock().unwrap_or_else(|p| p.into_inner());
            if store.latest_agent_run(agent.id)? != *expected {
                return Err(
                    "A newer run changed this agent before automatic reconciliation".into(),
                );
            }
            if store.agent(agent.id)? != agent || store.pending_agent_launch(agent.id)?.is_some() {
                return Err("The historical agent changed before its retirement".into());
            }
            if store.agent_is_interactive(agent.id)? {
                return Ok(Reconciliation::UserOwned);
            }
            // The transaction repeats these fences against the scheduler's
            // independent WAL connection before any authority is revoked.
            if !store.retire_completed_automation_process(&agent, expected, now())? {
                return Err("The historical agent changed before its retirement".into());
            }
            drop(store);
            drop(_reservation);
            announce(host, &registry);
            return Ok(Reconciliation::Closed);
        }
        let handle = handle.ok_or(
            "The saved agent is not attached. Open its latest run to restore or close the agent, then retry this action",
        )?;
        let idle_generation = if !handle.is_completed() {
            let snapshot = handle
                .snapshot(None)
                .ok_or("Provider state is unavailable")?;
            if snapshot.ready && snapshot.busy && !snapshot.waiting && snapshot.requests.is_empty()
            {
                return Ok(Reconciliation::Busy);
            }
            if !snapshot.ready || snapshot.waiting || !snapshot.requests.is_empty() {
                return Err("The provider is not idle; automatic cleanup did not stop it".into());
            }
            Some(snapshot.generation)
        } else if !retire {
            return Err("The provider ended and cannot accept another scheduled prompt".into());
        } else {
            None
        };
        if !retire {
            return Ok(Reconciliation::Idle);
        }
        if let Some(generation) = idle_generation {
            // An automation nobody took part in leaves nothing running.
            handle.decide_detached_work(super::detached_work_on_close(host, source.id));
            if !handle.try_close_if_idle(generation)? {
                return Ok(Reconciliation::Busy);
            }
        }
        super::super::chat::process(
            host,
            EXECUTION_CLIENT,
            ClientRequest::ChatClose { chat_id: source.id },
        )
        .map_err(|error| error.message)?;
    } else if !retire {
        // A conclusive pre-provider failure has nothing to reuse. Retire only
        // its exact empty dispatch, so normal scheduler release can clear the
        // reusable slot without replaying the failed run. Pending or uncertain
        // startup still refuses above and the write transaction repeats every
        // ownership/run predicate before publishing the ordinary retirement.
        registry
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .close_unstarted_automation_agent(&agent, expected)?;
        announce(host, &registry);
        return Ok(Reconciliation::Closed);
    }
    registry
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .close_agent(&agent.origin, agent.id)?;
    announce(host, &registry);
    Ok(Reconciliation::Closed)
}

/// Explicit scheduler cancellation targets a provider-confirmed RunId, never
/// whichever turn happens to be active when its interrupt reaches the actor.
pub(in crate::host) fn cancel_agent_run(host: &SharedHost, expected: &Run) -> Result<(), String> {
    let control = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .control_gate
        .clone();
    let _control = control.read().unwrap_or_else(|p| p.into_inner());
    cancel_agent_run_under_control(host, expected)
}

/// The RPC dispatcher already owns control.read(). Do not acquire it again:
/// a queued shutdown writer can make recursive read acquisition deadlock.
pub(in crate::host) fn cancel_agent_run_under_control(
    host: &SharedHost,
    expected: &Run,
) -> Result<(), String> {
    super::super::client::ensure_control_open(host).map_err(|error| error.message)?;
    let gate = agent_gate(host, expected.agent);
    let mut dispatch = gate.lock().unwrap_or_else(|p| p.into_inner());
    let registry = registry(host)?;
    let agent = {
        let registry = registry.lock().unwrap_or_else(|p| p.into_inner());
        let agent = registry.agent(expected.agent)?;
        if agent.closed
            || !matches!(agent.origin, Origin::Automation { .. })
            || registry.agent_is_interactive(agent.id)?
        {
            return Err("This automation no longer owns the provider controls".into());
        }
        if registry.latest_agent_run(agent.id)? != *expected || expected.state.is_terminal() {
            return Err("The requested active run changed before cancellation".into());
        }
        agent
    };
    if withdraw_undelivered(host, &mut dispatch, Some(expected.id))? {
        return Ok(());
    }
    let Some(HostedSession::Chat(source)) = agent.session else {
        return Err("This accepted run has no provider-confirmed actor to interrupt".into());
    };
    let handle = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .chats
        .get(&source.id)
        .filter(|record| {
            HostedSession::Chat(source.clone())
                .same_process(&HostedSession::Chat(record.session.clone()))
        })
        .map(|record| record.handle.clone())
        .ok_or("The exact provider ownership is unavailable for cancellation")?;
    handle.interrupt_run(expected.id)
}
