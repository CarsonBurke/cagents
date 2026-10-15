//! Serialize automatic cleanup with interactive ownership changes. Displaying
//! a conversation is not takeover; issuing work or opening native controls is.
use super::*;
use crate::terminal_ipc::{ProtocolError, ProtocolErrorCode};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::host) enum Reconciliation {
    Closed,
    Idle,
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
        ClientRequest::ChatCloseSession { session } => (session.id, false),
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
    let _dispatch = gate.lock().unwrap_or_else(|p| p.into_inner());
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
    let _dispatch = gate.lock().unwrap_or_else(|p| p.into_inner());
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
        if registry.pending_agent_launch(agent.id)?.is_some() {
            return Err("Provider startup ownership remains unconfirmed".into());
        }
        agent
    };
    if let Some(source) = &agent.session {
        let HostedSession::Chat(source) = source else {
            return Err("This agent has no native cleanup contract".into());
        };
        // Keep the existing control -> Agent -> chat lifecycle lock order.
        // A retained helper can restart its backend inside the same Handle and
        // execution_instance. Capture completion/idle eligibility only after
        // claiming that transition, and retain it through teardown.
        #[cfg(test)]
        {
            let hook = BEFORE_RECONCILIATION_TRANSITION.lock().unwrap().take();
            if let Some(hook) = hook { hook(); }
        }
        let transition = super::super::chat::presentation_close::transition_gate(host, source.id);
        let transition = match transition.try_lock() {
            Ok(guard) => guard,
            Err(std::sync::TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => {
                return Err("This exact conversation has a lifecycle transition in progress; automatic cleanup was deferred".into());
            }
        };
        let (handle, execution_instance) = {
            let state = host.lock().unwrap_or_else(|p| p.into_inner());
            let record = state
                .chats
                .get(&source.id)
                .filter(|record| {
                    HostedSession::Chat(source.clone())
                        .same_process(&HostedSession::Chat(record.session.clone()))
                })
                .ok_or("The exact provider ownership is unavailable for reconciliation")?;
            (record.handle.clone(), record.execution_instance)
        };
        let idle_generation = if !handle.is_completed() {
            let snapshot = handle
                .snapshot(None)
                .ok_or("Provider state is unavailable")?;
            if !snapshot.ready || snapshot.busy || snapshot.waiting || !snapshot.requests.is_empty()
            {
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
        #[cfg(test)]
        {
            let hook = AFTER_RECONCILIATION_ELIGIBILITY.lock().unwrap().take();
            if let Some(hook) = hook { hook(); }
        }
        if let Some(generation) = idle_generation {
            handle.close_if_idle(generation)?;
        }
        super::super::chat::close_captured_under_transition(
            host,
            EXECUTION_CLIENT,
            source,
            &handle,
            execution_instance,
            &transition,
        )
        .map_err(|error| error.message)?;
    } else if !retire {
        return Err("The agent never established a reusable provider".into());
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
    let _dispatch = gate.lock().unwrap_or_else(|p| p.into_inner());
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

// Scheduling seam before the real lifecycle claim. The eligibility snapshot
// is deliberately captured after this seam while the claim remains held.
#[cfg(test)]
static BEFORE_RECONCILIATION_TRANSITION: std::sync::Mutex<Option<Box<dyn FnOnce() + Send>>> = std::sync::Mutex::new(None);
#[cfg(test)]
pub(in crate::host) fn before_reconciliation_transition(hook: impl FnOnce() + Send + 'static) {
    *BEFORE_RECONCILIATION_TRANSITION.lock().unwrap() = Some(Box::new(hook));
}

// Exact old race boundary: eligibility has been observed, but close has not
// begun. The real lifecycle claim must already exclude supported restarts.
#[cfg(test)]
static AFTER_RECONCILIATION_ELIGIBILITY: std::sync::Mutex<Option<Box<dyn FnOnce() + Send>>> = std::sync::Mutex::new(None);
#[cfg(test)]
pub(in crate::host) fn after_reconciliation_eligibility(hook: impl FnOnce() + Send + 'static) {
    *AFTER_RECONCILIATION_ELIGIBILITY.lock().unwrap() = Some(Box::new(hook));
}
