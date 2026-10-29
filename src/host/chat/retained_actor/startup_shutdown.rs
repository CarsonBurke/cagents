//! Private, exact close-only authority for the ordinary-exit coordinator.
//!
//! Captures never grant installation, input, history or account access. All
//! existing semantic close and retirement checks remain in the shared path.
use super::*;
use crate::chat::protocol::{LaunchOwnerKind, LaunchOwnerToken};

#[derive(Clone)]
struct StartupBinding {
    source: Session,
    requested: Session,
    context: Arc<chat::protocol::ExecutionContext>,
    token: LaunchOwnerToken,
    identity: chat::runtime::ActorIdentity,
    failed_close: Option<super::super::CloseTarget>,
    // Keep the original startup control alive until semantic adoption. In
    // particular, an uninstalled helper must not disappear through EOF here.
    _remote: Option<Handle>,
}

pub(super) fn same_target(
    left: &Option<super::super::CloseTarget>,
    right: &Option<super::super::CloseTarget>,
) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) => {
            left.source == right.source
                && left.instance == right.instance
                && left.handle.same_attachment(&right.handle)
        }
        _ => false,
    }
}

impl StartupBinding {
    fn capture(pending: &Startup) -> Result<Self, String> {
        let snapshot = pending.context.ownership_snapshot();
        let LaunchOwnerKind::RemoteActor { identity } = snapshot.kind else {
            return Err("This startup has no transferred semantic actor".into());
        };
        let token = snapshot
            .token
            .ok_or("The transferred startup lost its exact owner generation")?;
        Ok(Self {
            source: pending.source.clone(),
            requested: pending.requested.clone(),
            context: pending.context.clone(),
            token,
            identity,
            failed_close: pending.failed_close.clone(),
            _remote: snapshot.remote,
        })
    }

    fn matches(&self, pending: &Startup) -> bool {
        if pending.source != self.source
            || pending.requested != self.requested
            || !Arc::ptr_eq(&pending.context, &self.context)
            || !same_target(&pending.failed_close, &self.failed_close)
        {
            return false;
        }
        self.owner_is_current()
    }

    fn owner_is_current(&self) -> bool {
        let snapshot = self.context.ownership_snapshot();
        snapshot.token == Some(self.token)
            && snapshot.kind
                == (LaunchOwnerKind::RemoteActor {
                    identity: self.identity.clone(),
                })
    }

    fn require_state(&self, state: &HostState) -> Result<(), String> {
        if state
            .native_actor_startups
            .get(&self.source.id)
            .is_none_or(|pending| !self.matches(pending))
        {
            return Err(
                "The captured startup source, context, actor or failed attachment changed".into(),
            );
        }
        Ok(())
    }

    fn require_inspection(
        &self,
        identity: &chat::runtime::ActorIdentity,
        issued: &Session,
        aliases: &[Session],
    ) -> Result<(), String> {
        if identity != &self.identity || (issued != &self.source && !aliases.contains(&self.source))
        {
            return Err("The authenticated helper does not own this exact captured startup".into());
        }
        Ok(())
    }

    fn forget_proved(&self, host: &SharedHost) -> Result<(), String> {
        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
        self.require_state(&state)?;
        if state.shutting_down
            || self.context.outcome() != chat::protocol::LaunchOutcome::TeardownProved
        {
            return Err(
                "The exact startup remains retained until its complete cleanup proof".into(),
            );
        }
        state.native_actor_startups.remove(&self.source.id);
        state.inventory_generation += 1;
        drop(state);
        crate::daemon::wake();
        Ok(())
    }
}

#[derive(Clone)]
struct ExecutionBinding {
    agent: crate::model::AgentId,
    source: Session,
    context: Arc<chat::protocol::ExecutionContext>,
    token: LaunchOwnerToken,
    identity: chat::runtime::ActorIdentity,
    _remote: Option<Handle>,
}

impl ExecutionBinding {
    fn capture(
        state: &HostState,
        agent: crate::model::AgentId,
        source: &Session,
        context: &Arc<chat::protocol::ExecutionContext>,
    ) -> Result<Self, String> {
        let snapshot = context.ownership_snapshot();
        let LaunchOwnerKind::RemoteActor { identity } = snapshot.kind else {
            return Err("The execution startup has no transferred semantic actor".into());
        };
        let binding = Self {
            agent,
            source: source.clone(),
            context: context.clone(),
            token: snapshot
                .token
                .ok_or("The execution startup lost its owner generation")?,
            identity,
            _remote: snapshot.remote,
        };
        binding.require_state(state)?;
        Ok(binding)
    }

    fn require_state(&self, state: &HostState) -> Result<(), String> {
        let snapshot = self.context.ownership_snapshot();
        if state
            .execution_startups
            .get(&self.agent)
            .is_none_or(|(intent, context)| {
                intent != &crate::agent_execution::HostedSession::Chat(self.source.clone())
                    || !Arc::ptr_eq(context, &self.context)
            })
            || snapshot.token != Some(self.token)
            || snapshot.kind
                != (LaunchOwnerKind::RemoteActor {
                    identity: self.identity.clone(),
                })
        {
            return Err("The captured execution launch, context or semantic actor changed".into());
        }
        Ok(())
    }

    fn native_binding(&self, state: &HostState) -> Result<Option<StartupBinding>, String> {
        let Some(pending) = state.native_actor_startups.get(&self.source.id) else {
            return Ok(None);
        };
        if (pending.source != self.source && pending.requested != self.source)
            || !Arc::ptr_eq(&pending.context, &self.context)
        {
            return Err("A different native startup owns this execution descriptor".into());
        }
        let binding = StartupBinding::capture(pending)?;
        if binding.token != self.token || binding.identity != self.identity {
            return Err("The native and execution startup owners differ".into());
        }
        Ok(Some(binding))
    }
}

/// This value is deliberately private to HOST and has no wire representation.
/// Only an ordinary-exit transaction can mint it; every publication checks the
/// same original proof and current exact attachment or startup owner again.
#[derive(Clone)]
pub(in crate::host) struct Capture {
    proof: HostSessionProof,
    source: Session,
    requested: Session,
    identity: chat::runtime::ActorIdentity,
    startup: Option<StartupBinding>,
    execution: Option<ExecutionBinding>,
    target: Option<super::super::CloseTarget>,
}

pub(in crate::host) fn capture_startup(
    state: &HostState,
    pending: &Startup,
) -> Result<Capture, String> {
    let startup = StartupBinding::capture(pending)?;
    startup.require_state(state)?;
    let target = match state.chats.get(&pending.source.id) {
        Some(record) => {
            if record.handle.actor_identity().as_ref() != Some(&startup.identity)
                || record.execution_instance != startup.identity.instance
                || startup
                    .failed_close
                    .as_ref()
                    .is_some_and(|failed| !failed.matches(record))
            {
                return Err("The startup's current semantic attachment was replaced".into());
            }
            Some(super::super::CloseTarget::from_record(record))
        }
        None if startup.failed_close.is_some() => {
            return Err("The failed startup lost its exact semantic attachment".into());
        }
        None => None,
    };
    let capture = Capture {
        proof: state.proof().clone(),
        source: pending.source.clone(),
        requested: pending.requested.clone(),
        identity: startup.identity.clone(),
        startup: Some(startup),
        execution: None,
        target,
    };
    // Cancellation is requested outside HOST after capture, so the latch is
    // checked when the capability is used rather than while it is minted.
    capture.require_ordinary_intent(state)?;
    Ok(capture)
}

pub(in crate::host) fn capture_hosted(
    state: &HostState,
    source: &Session,
    handle: &Handle,
) -> Result<Capture, String> {
    let record = state
        .chats
        .get(&source.id)
        .filter(|record| record.session == *source && record.handle.same_attachment(handle))
        .ok_or("The hosted cleanup attachment changed before capture")?;
    let identity = handle
        .actor_identity()
        .ok_or("The hosted attachment is not a retained semantic actor")?;
    if identity.instance != record.execution_instance {
        return Err("The hosted semantic actor generation changed".into());
    }
    let startup = state
        .native_actor_startups
        .get(&source.id)
        .map(StartupBinding::capture)
        .transpose()?;
    if startup.as_ref().is_some_and(|startup| {
        startup.identity != identity
            || startup
                .failed_close
                .as_ref()
                .is_some_and(|failed| !failed.matches(record))
    }) {
        return Err("The hosted attachment does not match its captured startup actor".into());
    }
    let capture = Capture {
        proof: state.proof().clone(),
        source: source.clone(),
        requested: startup
            .as_ref()
            .map_or_else(|| source.clone(), |startup| startup.requested.clone()),
        identity,
        startup,
        execution: None,
        target: Some(super::super::CloseTarget::from_record(record)),
    };
    capture.require_ordinary_intent(state)?;
    Ok(capture)
}

pub(in crate::host) fn capture_execution(
    state: &HostState,
    agent: crate::model::AgentId,
    intent: &crate::agent_execution::HostedSession,
    context: &Arc<chat::protocol::ExecutionContext>,
) -> Result<Capture, String> {
    let crate::agent_execution::HostedSession::Chat(source) = intent else {
        return Err("A terminal launch has no retained semantic actor".into());
    };
    let execution = ExecutionBinding::capture(state, agent, source, context)?;
    let startup = execution.native_binding(state)?;
    let target = state
        .chats
        .get(&source.id)
        .map(|record| -> Result<super::super::CloseTarget, String> {
            if record.handle.actor_identity().as_ref() != Some(&execution.identity)
                || record.execution_instance != execution.identity.instance
                || startup
                    .as_ref()
                    .and_then(|startup| startup.failed_close.as_ref())
                    .is_some_and(|failed| !failed.matches(record))
            {
                return Err("The execution startup's semantic attachment changed".into());
            }
            Ok(super::super::CloseTarget::from_record(record))
        })
        .transpose()?;
    if target.is_none()
        && startup
            .as_ref()
            .is_some_and(|startup| startup.failed_close.is_some())
    {
        return Err("The execution startup lost its exact failed attachment".into());
    }
    let capture = Capture {
        proof: state.proof().clone(),
        source: startup
            .as_ref()
            .map_or_else(|| source.clone(), |startup| startup.source.clone()),
        requested: source.clone(),
        identity: execution.identity.clone(),
        startup,
        execution: Some(execution),
        target,
    };
    capture.require_ordinary_intent(state)?;
    Ok(capture)
}

impl Capture {
    pub(in crate::host) fn identity(&self) -> &chat::runtime::ActorIdentity {
        &self.identity
    }

    pub(in crate::host) fn source(&self) -> &Session {
        &self.source
    }

    pub(in crate::host) fn requested(&self) -> &Session {
        &self.requested
    }

    pub(in crate::host) fn matches_startup(&self, pending: &Startup) -> bool {
        self.startup
            .as_ref()
            .is_some_and(|startup| startup.matches(pending))
    }

    fn require_ordinary_intent(&self, state: &HostState) -> Result<(), String> {
        if !state.shutting_down
            || !state.ordinary_exit_cleanup_pending()
            || state.has_committed_upgrade()
            || state.proof() != &self.proof
        {
            return Err("The original ordinary-shutdown ownership changed".into());
        }
        Ok(())
    }

    pub(in crate::host::chat) fn require_state(&self, state: &HostState) -> Result<(), String> {
        self.require_ordinary_intent(state)?;
        if let Some(startup) = &self.startup {
            startup.require_state(state)?;
            if !startup.context.ownership_snapshot().ordinary_requested {
                return Err("The original startup cancellation has not been requested".into());
            }
        }
        if let Some(execution) = &self.execution {
            execution.require_state(state)?;
            if !execution.context.ownership_snapshot().ordinary_requested {
                return Err("The original execution cancellation has not been requested".into());
            }
            if self.startup.is_none() && state.native_actor_startups.contains_key(&self.source.id) {
                return Err("A different native startup appeared after execution capture".into());
            }
        }
        if self.target.as_ref().is_some_and(|target| {
            state
                .chats
                .get(&target.source.id)
                .is_none_or(|record| !target.matches(record))
        }) {
            return Err("The captured current semantic attachment changed".into());
        }
        if state.chats.get(&self.source.id).is_some_and(|record| {
            record.execution_instance != self.identity.instance
                || record.handle.actor_identity().as_ref() != Some(&self.identity)
        }) {
            return Err("The captured semantic actor was replaced".into());
        }
        Ok(())
    }

    fn cleanup_source(&self) -> &Session {
        self.target
            .as_ref()
            .map_or(&self.source, |target| &target.source)
    }

    pub(in crate::host::chat) fn require_reserved_state(
        &self,
        state: &HostState,
    ) -> Result<(), String> {
        self.require_state(state)?;
        let source = self.cleanup_source();
        if state.chat_spawning.get(&source.id) != Some(source) {
            return Err("The exact coordinator-owned cleanup reservation changed".into());
        }
        Ok(())
    }

    pub(super) fn require_reserved_host(&self, host: &SharedHost) -> Result<(), String> {
        self.require_reserved_state(&host.lock().unwrap_or_else(|p| p.into_inner()))
    }

    pub(super) fn require_host(&self, host: &SharedHost) -> Result<(), String> {
        self.require_state(&host.lock().unwrap_or_else(|p| p.into_inner()))
    }

    pub(super) fn inherit_original_helper(&self, handle: &Handle) -> Result<(), String> {
        for previous in [
            self.startup
                .as_ref()
                .and_then(|startup| startup._remote.as_ref()),
            self.execution
                .as_ref()
                .and_then(|execution| execution._remote.as_ref()),
        ]
        .into_iter()
        .flatten()
        {
            handle.inherit_original_helper_from(previous)?;
        }
        Ok(())
    }

    pub(super) fn require_inspection(
        &self,
        identity: &chat::runtime::ActorIdentity,
        issued: &Session,
        aliases: &[Session],
    ) -> Result<(), String> {
        if identity != &self.identity {
            return Err(
                "The authenticated semantic actor differs from its shutdown capture".into(),
            );
        }
        if let Some(startup) = &self.startup {
            startup.require_inspection(identity, issued, aliases)?;
        } else if issued != &self.source && !aliases.contains(&self.source) {
            return Err("The authenticated actor does not recognize its captured source".into());
        }
        Ok(())
    }

    pub(super) fn enroll_uninstalled(
        &self,
        host: &SharedHost,
        identity: &chat::runtime::ActorIdentity,
        issued: &Session,
        aliases: &[Session],
        scope: Result<&crate::account::scope::LaunchAccountScope, &str>,
        installed: bool,
    ) -> Result<(), String> {
        self.require_host(host)?;
        self.require_inspection(identity, issued, aliases)?;
        if self.startup.is_none() && self.execution.is_none() {
            // Ordinary hosted cleanup never gains a missing ownership row.
            return verify_close_ownership(host, identity, issued, aliases, scope).map(|_| ());
        }
        enroll_pending(host, identity, issued, aliases, scope, installed)
    }
}

/// The coordinator runs this once per exact ActorIdentity, using a current
/// hosted attachment when one exists. Completion proves semantic settlement;
/// the original helper's separate Child.wait may still need to be polled.
pub(in crate::host) fn cleanup(host: &SharedHost, capture: &Capture) -> Result<(), String> {
    capture.require_host(host)?;
    let requested = capture.cleanup_source();
    close_with_target(
        host,
        requested,
        Some(capture.identity.record.clone()),
        capture.target.as_ref(),
        None,
        Some(capture),
        true,
    )
}

/// Normal pending Agent Close and retry share semantic cleanup without gaining
/// an ordinary-shutdown admission exemption. Exact retired capsules remain
/// usable after live discovery disappears; no disappearance is itself proof.
pub(super) fn cleanup_normal_startup(host: &SharedHost, pending: &Startup) -> Result<(), String> {
    let binding = StartupBinding::capture(pending)?;
    cleanup_normal_bound(host, &binding, |state| {
        binding.require_state(state)?;
        if binding.failed_close.as_ref().is_some_and(|failed| {
            state
                .chats
                .get(&binding.source.id)
                .is_some_and(|record| !failed.matches(record))
        }) {
            return Err("The failed startup's exact semantic attachment changed".into());
        }
        Ok(())
    })?;
    binding.forget_proved(host)
}

/// An execution intent can outlive normal forget_startup after its durable
/// clear failed. Its original context and exact AgentId remain cleanup custody
/// even without a native startup row or a current presentation attachment.
pub(in crate::host) fn cleanup_execution_startup(
    host: &SharedHost,
    agent: crate::model::AgentId,
    source: &Session,
    context: &Arc<chat::protocol::ExecutionContext>,
) -> Result<(), String> {
    let (execution, native) = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        let execution = ExecutionBinding::capture(&state, agent, source, context)?;
        let native = execution.native_binding(&state)?;
        (execution, native)
    };
    let binding = native.clone().unwrap_or_else(|| StartupBinding {
        source: source.clone(),
        requested: source.clone(),
        context: context.clone(),
        token: execution.token,
        identity: execution.identity.clone(),
        failed_close: None,
        _remote: execution._remote.clone(),
    });
    cleanup_normal_bound(host, &binding, |state| {
        execution.require_state(state)?;
        match &native {
            Some(native) => native.require_state(state),
            None if state.native_actor_startups.contains_key(&source.id) => {
                Err("A different native startup appeared during execution cleanup".into())
            }
            None => Ok(()),
        }
    })?;
    if let Some(native) = native {
        native.forget_proved(host)?;
    }
    Ok(())
}

fn cleanup_normal_bound(
    host: &SharedHost,
    binding: &StartupBinding,
    require_owner: impl Fn(&HostState) -> Result<(), String>,
) -> Result<(), String> {
    let proof = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        require_owner(&state)?;
        if !binding.owner_is_current()
            || state.shutting_down
            || state.chat_spawning.contains_key(&binding.source.id)
        {
            return Err("The pending startup is still transitioning or shutting down".into());
        }
        state.proof().clone()
    };
    if matches!(
        Handle::inspect_retained_retirement(&binding.identity.record, &proof)?,
        RetirementInspection::Running
    ) {
        let inspection = Handle::inspect_remote(&binding.identity.record, proof.clone())?;
        binding.require_inspection(
            &inspection.identity,
            &inspection.source,
            &inspection.previous_sources,
        )?;
        {
            let state = host.lock().unwrap_or_else(|p| p.into_inner());
            require_owner(&state)?;
            if !binding.owner_is_current()
                || state.proof() != &proof
                || state.shutting_down
                || state.chat_spawning.contains_key(&binding.source.id)
            {
                return Err("The exact pending startup changed before cleanup enrollment".into());
            }
        }
        enroll_pending(
            host,
            &inspection.identity,
            &inspection.source,
            &inspection.previous_sources,
            inspection
                .launch_account_scope
                .as_ref()
                .map_err(String::as_str),
            inspection.state.installed,
        )?;
    }
    close_with_target(
        host,
        &binding.source,
        Some(binding.identity.record.clone()),
        None,
        None,
        None,
        true,
    )?;
    let state = host.lock().unwrap_or_else(|p| p.into_inner());
    require_owner(&state)?;
    if state.proof() != &proof || state.shutting_down || !binding.owner_is_current() {
        return Err("The exact launch owner changed after semantic retirement".into());
    }
    drop(state);
    if !binding
        .context
        .complete_semantic_cleanup(binding.token, &binding.identity)
    {
        return Err(
            "Semantic startup retirement committed; the original helper wait remains pending"
                .into(),
        );
    }
    Ok(())
}

#[cfg(test)]
#[path = "startup_shutdown_tests.rs"]
mod tests;
