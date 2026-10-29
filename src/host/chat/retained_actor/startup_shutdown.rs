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
        if pending.failed_close.as_ref().is_some_and(|failed| {
            failed.instance != identity.instance
                || failed.source.id != pending.source.id
                || failed.handle.actor_identity().as_ref() != Some(&identity)
        }) {
            return Err("The failed startup target belongs to another exact semantic actor".into());
        }
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
        if self.failed_close.as_ref().is_some_and(|failed| {
            state
                .chats
                .get(&self.source.id)
                .is_some_and(|record| !failed.matches(record))
        }) {
            return Err("The failed startup's exact semantic attachment changed".into());
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
        if state
            .execution_startups
            .get(&self.agent)
            .is_none_or(|(intent, context)| !self.matches(self.agent, intent, context))
        {
            return Err("The captured execution launch, context or semantic actor changed".into());
        }
        Ok(())
    }

    fn matches(
        &self,
        agent: crate::model::AgentId,
        intent: &crate::agent_execution::HostedSession,
        context: &Arc<chat::protocol::ExecutionContext>,
    ) -> bool {
        let snapshot = self.context.ownership_snapshot();
        agent == self.agent
            && intent == &crate::agent_execution::HostedSession::Chat(self.source.clone())
            && Arc::ptr_eq(context, &self.context)
            && snapshot.token == Some(self.token)
            && snapshot.kind
                == (LaunchOwnerKind::RemoteActor {
                    identity: self.identity.clone(),
                })
    }

    fn require_inspection(
        &self,
        identity: &chat::runtime::ActorIdentity,
        issued: &Session,
        aliases: &[Session],
    ) -> Result<(), String> {
        if identity != &self.identity || (issued != &self.source && !aliases.contains(&self.source))
        {
            return Err(
                "The authenticated helper does not recognize a captured execution intent source"
                    .into(),
            );
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

fn context_actor(
    context: &Arc<chat::protocol::ExecutionContext>,
    identity: &chat::runtime::ActorIdentity,
) -> bool {
    matches!(context.ownership_snapshot().kind, LaunchOwnerKind::RemoteActor { identity: current } if &current == identity)
}

/// All same-actor obligations are frozen together before hosted-first job
/// coalescing can discard the other entry points. Same TerminalId alone never
/// joins owners, and a terminal intent never gains native cleanup authority.
fn collect_actor_bindings(
    state: &HostState,
    identity: &chat::runtime::ActorIdentity,
) -> Result<(Vec<StartupBinding>, Vec<ExecutionBinding>), String> {
    let mut startups = Vec::new();
    for (id, pending) in &state.native_actor_startups {
        if context_actor(&pending.context, identity) {
            if *id != pending.source.id {
                return Err("The native startup map key differs from its exact source".into());
            }
            let binding = StartupBinding::capture(pending)?;
            binding.require_state(state)?;
            startups.push(binding);
        }
    }
    let mut executions = Vec::new();
    for (agent, (intent, context)) in &state.execution_startups {
        if context_actor(context, identity) {
            let crate::agent_execution::HostedSession::Chat(source) = intent else {
                return Err(
                    "A retained semantic actor has an incompatible terminal execution intent"
                        .into(),
                );
            };
            executions.push(ExecutionBinding::capture(state, *agent, source, context)?);
        }
    }
    Ok((startups, executions))
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
    startups: Vec<StartupBinding>,
    executions: Vec<ExecutionBinding>,
    target: Option<super::super::CloseTarget>,
    /// A normal close can retire its HOST record before the original child
    /// wait completes. Preserve that exact failed attachment, but permit only
    /// its verified retirement capsule; no live attachment may replace it.
    retired_target: Option<super::super::CloseTarget>,
}

pub(in crate::host) fn capture_startup(
    state: &HostState,
    pending: &Startup,
) -> Result<Capture, String> {
    let startup = StartupBinding::capture(pending)?;
    startup.require_state(state)?;
    let (startups, executions) = collect_actor_bindings(state, &startup.identity)?;
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
        None => None,
    };
    let retired_target = target
        .is_none()
        .then(|| {
            startups
                .iter()
                .find_map(|startup| startup.failed_close.clone())
        })
        .flatten();
    let capture = Capture {
        proof: state.proof().clone(),
        source: pending.source.clone(),
        requested: pending.requested.clone(),
        identity: startup.identity.clone(),
        startups,
        executions,
        target,
        retired_target,
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
    let (startups, executions) = collect_actor_bindings(state, &identity)?;
    let capture = Capture {
        proof: state.proof().clone(),
        source: source.clone(),
        requested: startup
            .as_ref()
            .map_or_else(|| source.clone(), |startup| startup.requested.clone()),
        identity,
        startups,
        executions,
        target: Some(super::super::CloseTarget::from_record(record)),
        retired_target: None,
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
    let (startups, executions) = collect_actor_bindings(state, &execution.identity)?;
    if state
        .native_actor_startups
        .get(&source.id)
        .is_some_and(|pending| !context_actor(&pending.context, &execution.identity))
    {
        return Err("A different actor's native startup owns this execution descriptor".into());
    }
    let startup = startups.iter().find(|startup| {
        Arc::ptr_eq(&startup.context, context)
            && (startup.source == *source || startup.requested == *source)
    });
    let target = state
        .chats
        .get(&source.id)
        .map(|record| -> Result<super::super::CloseTarget, String> {
            if record.handle.actor_identity().as_ref() != Some(&execution.identity)
                || record.execution_instance != execution.identity.instance
                || startups
                    .iter()
                    .filter_map(|startup| startup.failed_close.as_ref())
                    .any(|failed| !failed.matches(record))
            {
                return Err("The execution startup's semantic attachment changed".into());
            }
            Ok(super::super::CloseTarget::from_record(record))
        })
        .transpose()?;
    let retired_target = target
        .is_none()
        .then(|| {
            startups
                .iter()
                .find_map(|startup| startup.failed_close.clone())
        })
        .flatten();
    let capture = Capture {
        proof: state.proof().clone(),
        source: startup
            .as_ref()
            .map_or_else(|| source.clone(), |startup| startup.source.clone()),
        requested: source.clone(),
        identity: execution.identity.clone(),
        startups,
        executions,
        target,
        retired_target,
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
        self.startups.iter().any(|startup| startup.matches(pending))
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

    fn require_frozen_bindings(
        &self,
        state: &HostState,
        context: Option<&Arc<chat::protocol::ExecutionContext>>,
    ) -> Result<usize, String> {
        let mut matched = 0;
        for startup in self
            .startups
            .iter()
            .filter(|startup| context.is_none_or(|context| Arc::ptr_eq(context, &startup.context)))
        {
            startup.require_state(state)?;
            if !startup.context.ownership_snapshot().ordinary_requested {
                return Err("The original startup cancellation has not been requested".into());
            }
            matched += 1;
        }
        for execution in self.executions.iter().filter(|execution| {
            context.is_none_or(|context| Arc::ptr_eq(context, &execution.context))
        }) {
            execution.require_state(state)?;
            if !execution.context.ownership_snapshot().ordinary_requested {
                return Err("The original execution cancellation has not been requested".into());
            }
            matched += 1;
        }
        Ok(matched)
    }

    fn contains_context(&self, context: &Arc<chat::protocol::ExecutionContext>) -> bool {
        self.startups
            .iter()
            .any(|binding| Arc::ptr_eq(&binding.context, context))
            || self
                .executions
                .iter()
                .any(|binding| Arc::ptr_eq(&binding.context, context))
    }

    /// Compare the current map census with the originally frozen set. A new
    /// AgentId using an old Arc is a new obligation even if actor/token match.
    /// The per-context proof gate ignores other contexts already settled by
    /// the coordinator, while the semantic operation validates the whole actor.
    fn require_current_census(
        &self,
        state: &HostState,
        context: Option<&Arc<chat::protocol::ExecutionContext>>,
    ) -> Result<(), String> {
        for (id, pending) in &state.native_actor_startups {
            let relevant = match context {
                Some(context) => {
                    Arc::ptr_eq(context, &pending.context)
                        || self.startups.iter().any(|binding| {
                            binding.source.id == *id && Arc::ptr_eq(context, &binding.context)
                        })
                        || self.executions.iter().any(|binding| {
                            binding.source.id == *id && Arc::ptr_eq(context, &binding.context)
                        })
                }
                None => {
                    context_actor(&pending.context, &self.identity)
                        || self.contains_context(&pending.context)
                        || *id == self.source.id
                        || self.startups.iter().any(|binding| binding.source.id == *id)
                        || self
                            .executions
                            .iter()
                            .any(|binding| binding.source.id == *id)
                }
            };
            if relevant
                && !self
                    .startups
                    .iter()
                    .any(|binding| binding.source.id == *id && binding.matches(pending))
            {
                return Err(
                    "A native startup obligation appeared or changed after actor capture".into(),
                );
            }
        }
        for (agent, (intent, owner)) in &state.execution_startups {
            let relevant = match context {
                Some(context) => {
                    Arc::ptr_eq(context, owner)
                        || self.executions.iter().any(|binding| {
                            binding.agent == *agent && Arc::ptr_eq(context, &binding.context)
                        })
                }
                None => {
                    context_actor(owner, &self.identity)
                        || self.contains_context(owner)
                        || self
                            .executions
                            .iter()
                            .any(|binding| binding.agent == *agent)
                }
            };
            if relevant
                && !self
                    .executions
                    .iter()
                    .any(|binding| binding.matches(*agent, intent, owner))
            {
                return Err(
                    "An execution intent obligation appeared or changed after actor capture".into(),
                );
            }
        }
        Ok(())
    }

    /// Proof application follows semantic removal of the resident chat. Keep
    /// every original per-context map, descriptor, AgentId, Arc and typed owner
    /// fence, without requiring the already retired attachment to remain live.
    pub(in crate::host) fn require_context_state(
        &self,
        state: &HostState,
        context: &Arc<chat::protocol::ExecutionContext>,
    ) -> Result<(), String> {
        self.require_ordinary_intent(state)?;
        if self.require_frozen_bindings(state, Some(context))? == 0 {
            return Err(
                "This context has no original obligation in the actor cleanup capture".into(),
            );
        }
        self.require_current_census(state, Some(context))
    }

    pub(in crate::host::chat) fn require_state(&self, state: &HostState) -> Result<(), String> {
        self.require_ordinary_intent(state)?;
        self.require_frozen_bindings(state, None)?;
        self.require_current_census(state, None)?;
        if self.target.as_ref().is_some_and(|target| {
            state
                .chats
                .get(&target.source.id)
                .is_none_or(|record| !target.matches(record))
        }) {
            return Err("The captured current semantic attachment changed".into());
        }
        if self.retired_target.is_some() && state.chats.contains_key(&self.source.id) {
            return Err(
                "A HOST attachment appeared after exact failed-target retirement capture".into(),
            );
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
            .or(self.retired_target.as_ref())
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

    pub(super) fn require_live_adoption(&self) -> Result<(), String> {
        if self.retired_target.is_some() {
            return Err("The captured failed target permits verified retirement only; live adoption was refused".into());
        }
        Ok(())
    }

    pub(super) fn inherit_original_helper(&self, handle: &Handle) -> Result<(), String> {
        for startup in &self.startups {
            if let Some(previous) = &startup._remote {
                handle.inherit_original_helper_from(previous)?;
            }
            if let Some(failed) = &startup.failed_close {
                handle.inherit_original_helper_from(&failed.handle)?;
            }
        }
        for execution in &self.executions {
            if let Some(previous) = &execution._remote {
                handle.inherit_original_helper_from(previous)?;
            }
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
        if issued != &self.source && !aliases.contains(&self.source) {
            return Err("The authenticated actor does not recognize its captured source".into());
        }
        for startup in &self.startups {
            startup.require_inspection(identity, issued, aliases)?;
            if startup
                .failed_close
                .as_ref()
                .is_some_and(|failed| issued != &failed.source && !aliases.contains(&failed.source))
            {
                return Err(
                    "The authenticated actor does not recognize a captured failed target source"
                        .into(),
                );
            }
        }
        for execution in &self.executions {
            execution.require_inspection(identity, issued, aliases)?;
        }
        if self
            .retired_target
            .as_ref()
            .is_some_and(|failed| issued != &failed.source && !aliases.contains(&failed.source))
        {
            return Err(
                "The verified actor does not recognize the exact retired failed target".into(),
            );
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
        self.require_live_adoption()?;
        self.require_inspection(identity, issued, aliases)?;
        if self.startups.is_empty() && self.executions.is_empty() {
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
    if capture.retired_target.is_some() {
        require_verified_failed_retirement(
            capture,
            Handle::inspect_retained_retirement(&capture.identity.record, &capture.proof)?,
        )?;
    }
    let requested = capture.cleanup_source();
    close_with_target(
        host,
        requested,
        Some(capture.identity.record.clone()),
        capture.target.as_ref(),
        None,
        Some(capture),
        true,
        capture.retired_target.is_some(),
    )
}

/// Normal pending Agent Close and retry share semantic cleanup without gaining
/// an ordinary-shutdown admission exemption. Exact retired capsules remain
/// usable after live discovery disappears; no disappearance is itself proof.
pub(super) fn cleanup_normal_startup(host: &SharedHost, pending: &Startup) -> Result<(), String> {
    let binding = StartupBinding::capture(pending)?;
    cleanup_normal_bound(host, &binding, |state| binding.require_state(state))?;
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
    let (proof, retired_only, target) = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        require_owner(&state)?;
        if !binding.owner_is_current()
            || state.shutting_down
            || state.chat_spawning.contains_key(&binding.source.id)
        {
            return Err("The pending startup is still transitioning or shutting down".into());
        }
        let target = state
            .chats
            .get(&binding.source.id)
            .map(super::super::CloseTarget::from_record);
        if binding.failed_close.as_ref().is_some_and(|failed| {
            state
                .chats
                .get(&binding.source.id)
                .is_some_and(|record| !failed.matches(record))
        }) {
            return Err("The failed startup's exact semantic attachment changed".into());
        }
        (
            state.proof().clone(),
            binding.failed_close.is_some() && target.is_none(),
            target,
        )
    };
    let retirement = Handle::inspect_retained_retirement(&binding.identity.record, &proof)?;
    if retired_only {
        match &retirement {
            RetirementInspection::Verified(verified) => {
                let receipt = verified.receipt();
                binding.require_inspection(
                    &receipt.identity,
                    &receipt.source,
                    &receipt.previous_sources,
                )?;
                let failed = binding
                    .failed_close
                    .as_ref()
                    .expect("retired-only mode requires exact failed target");
                if receipt.source != failed.source
                    && !receipt.previous_sources.contains(&failed.source)
                {
                    return Err(
                        "The verified receipt does not recognize the exact failed startup target"
                            .into(),
                    );
                }
            }
            RetirementInspection::Running | RetirementInspection::ExitedUnproved => {
                return Err("The absent failed startup target requires exact verified retirement; live adoption was refused".into());
            }
        }
    }
    if matches!(retirement, RetirementInspection::Running) {
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
    let requested = target
        .as_ref()
        .or(binding.failed_close.as_ref())
        .map_or(&binding.source, |target| &target.source);
    close_with_target(
        host,
        requested,
        Some(binding.identity.record.clone()),
        target.as_ref(),
        None,
        None,
        true,
        retired_only,
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

fn require_verified_failed_retirement(
    capture: &Capture,
    retirement: RetirementInspection,
) -> Result<(), String> {
    let RetirementInspection::Verified(verified) = retirement else {
        return Err("The absent failed startup target requires exact verified retirement; live adoption was refused".into());
    };
    let receipt = verified.receipt();
    capture.require_inspection(
        &receipt.identity,
        &receipt.source,
        &receipt.previous_sources,
    )
}

#[cfg(test)]
#[path = "startup_shutdown_tests.rs"]
mod tests;
