use super::*;
use crate::agent_execution::{Agent, HostedSession};
use crate::chat::protocol::{ExecutionContext, LaunchOutcome};
use crate::host::{
    agent_execution::{self, SharedRegistry},
    model::Registry,
};
use crate::model::AgentId;
use crate::upgrade::handoff::ChatSource;

fn binding(
    host: &SharedHost,
    id: TerminalId,
) -> Result<Option<(SharedRegistry, Agent)>, ProtocolError> {
    let Ok(registry) = agent_execution::registry(host) else {
        return Ok(None);
    };
    let agent = registry
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .agent_for_session(id)
        .map_err(retryable_unavailable)?;
    Ok(agent.map(|agent| (registry, agent)))
}

pub(super) fn require_current_binding(
    host: &SharedHost,
    client: ClientId,
    session: &Session,
) -> Result<(), ProtocolError> {
    if let Some((_, agent)) = binding(host, session.id)?
        && let Some(HostedSession::Chat(bound)) = agent.session
        && bound.launch.provider == Provider::Codex
    {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        if bound.proof.as_ref() != Some(state.proof()) {
            return Err(upgrade_resolution::restart_refusal(&state, client, session)
                .unwrap_or_else(|| {
                    retryable_unavailable("Reconnect through the agent's saved conversation")
                }));
        }
    }
    Ok(())
}

/// Resolve may reopen the exact registered conversation after a daemon loss.
/// An unowned saved pane still has an attachment-only Resolve operation.
pub(in crate::host) fn restore(host: &SharedHost, source: &Session) -> Result<bool, ProtocolError> {
    if source.launch.provider != Provider::Codex {
        return Ok(false);
    }
    let Some((registry, agent)) = binding(host, source.id)? else {
        return Ok(false);
    };
    let starts = agent_execution::start_gate(host, agent.id);
    let _starting = starts.lock().unwrap_or_else(|p| p.into_inner());
    let gate = agent_execution::agent_gate(host, agent.id);
    let _dispatch = gate.lock().unwrap_or_else(|p| p.into_inner());
    let agent = registry
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .agent(agent.id)
        .map_err(retryable_unavailable)?;
    // A concurrent window may already have recovered the agent while we
    // waited. Its exact alias is checked by resolve_upgrade.
    let proof = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        if state.chats.contains_key(&source.id) {
            return Ok(true);
        }
        state.proof().clone()
    };
    let Some(HostedSession::Chat(previous)) = agent.session else {
        return Err(unavailable("The agent no longer has this conversation"));
    };
    if agent.closed
        || agent.console.is_some()
        || previous.terminal.is_some()
        || previous.native_continuity.retains_provider()
        || !previous.owns_codex_listener()
        || previous
            .launch
            .session_id
            .as_deref()
            .is_none_or(str::is_empty)
        || !previous
            .proof
            .as_ref()
            .is_some_and(HostSessionProof::is_complete)
        || previous.proof.as_ref() == Some(&proof)
        || !(source == &previous || chat::is_first_codex_identity_update(source, &previous))
    {
        return Err(retryable_unavailable(
            "The agent's exact saved Codex conversation is unavailable for reconnect",
        ));
    }
    reconcile_startup(host, &registry, agent.id).map_err(retryable_unavailable)?;
    if has_native_owner(&previous)? {
        return Err(retryable_unavailable(
            "The agent's earlier Codex holder still owns this conversation",
        ));
    }
    let opened = agent_execution::working_directory::for_conversation(&registry, &previous)
        .map_err(retryable_unavailable)?
        .ok_or_else(|| unavailable("The agent's working directory could not be pinned"))?;
    let context = Arc::new(ExecutionContext::new(Some(opened.directory.clone())));
    let mut successor = previous.clone();
    successor.proof = Some(proof);
    let successor = HostedSession::Chat(successor);
    let reconnect = Managed { registry, previous };
    host.lock()
        .unwrap_or_else(|p| p.into_inner())
        .execution_startups
        .insert(agent.id, (successor, context.clone()));
    let response = start_publishing(
        host,
        agent_execution::EXECUTION_CLIENT,
        reconnect.previous.clone(),
        None,
        false,
        None,
        Default::default(),
        Some(context),
        None,
        None,
        Some(&reconnect),
    );
    match response {
        Ok(_) => {
            host.lock()
                .unwrap_or_else(|p| p.into_inner())
                .execution_startups
                .remove(&agent.id);
            if let Err(error) = opened.started(&reconnect.registry) {
                tracing::warn!(agent = %agent.id, %error, "The agent's directory was not recorded");
            }
            Ok(true)
        }
        Err(mut failure) => {
            if let Err(cleanup) = reconcile_startup(host, &reconnect.registry, agent.id) {
                failure.message.push_str(&format!(
                    ". Reconnect cleanup is incomplete ({cleanup}); retry to finish cleanup"
                ));
            }
            Err(failure)
        }
    }
}

pub(super) struct Managed {
    pub(super) registry: SharedRegistry,
    previous: Session,
}

impl Managed {
    pub(super) fn thread(&self) -> &str {
        self.previous
            .launch
            .session_id
            .as_deref()
            .expect("named conversation")
    }

    pub(super) fn stop_previous(&self) -> Result<(), ProtocolError> {
        self.registry
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .stop_agent_process(
                &HostedSession::Chat(self.previous.clone()),
                chat::now_ms().unwrap_or(0),
            )
            .map_err(retryable_unavailable)?;
        Ok(())
    }

    pub(super) fn bind(
        &self,
        registry: &mut Registry,
        state: &mut HostState,
        session: &Session,
    ) -> Result<(), String> {
        if !chat::is_upgrade_recovery(&self.previous, session)
            || !registry.adopt_successor(&self.previous, session)?
        {
            return Err("The restarted Codex conversation could not take over the agent".into());
        }
        // A pane may have been attached just before the original thread ID
        // was recorded. Both descriptors identify the same prior provider.
        let mut issued = self.previous.clone();
        issued.launch.session_id = None;
        state
            .upgrade_chats
            .entry(session.id)
            .or_default()
            .push(ChatSource {
                source: issued,
                launch: self.previous.launch.clone(),
            });
        state.idle_adoptions.insert(session.id);
        Ok(())
    }
}

/// Reconcile an unpublished replacement while holding the agent's dispatch
/// gate. The old stopped marker says nothing about this newer provider.
pub(in crate::host) fn reconcile_startup(
    host: &SharedHost,
    registry: &SharedRegistry,
    agent: AgentId,
) -> Result<(), String> {
    let startup = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .execution_startups
        .get(&agent)
        .cloned();
    let Some((source, context)) = startup else {
        return Ok(());
    };
    if context.reconcile_cleanup()? == LaunchOutcome::Uncertain {
        return Err("The reconnect provider's teardown is still unconfirmed".into());
    }
    registry
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .retire_agent_authority(&source)?;
    host.lock()
        .unwrap_or_else(|p| p.into_inner())
        .execution_startups
        .remove(&agent);
    Ok(())
}
