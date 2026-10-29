//! One coalescing semantic-outcome broker for all helper-owned actors. Provider
//! callbacks only wake it; neither callback return nor enqueueing acknowledges
//! a durable outcome. No RPC, SQLite transaction or journal wait holds HostState.
use super::*;
use crate::chat::runtime::{RetirementInspection, VerifiedRetirement};
use crate::host::model::NativeActorEvent;
use std::{
    collections::BTreeSet,
    sync::{Condvar, Mutex},
};

pub(in crate::host) mod startup_shutdown;

#[derive(Clone)]
pub(in crate::host) struct Startup {
    pub(in crate::host) source: Session,
    pub(in crate::host) requested: Session,
    pub(in crate::host) context: Arc<chat::protocol::ExecutionContext>,
    /// An initial named resume failed after installation. Only this exact
    /// attachment may finish its normal semantic close before a retry starts.
    pub(in crate::host) failed_close: Option<super::CloseTarget>,
}

impl Startup {
    pub(in crate::host) fn matches(&self, captured: &Self) -> bool {
        self.source == captured.source
            && self.requested == captured.requested
            && Arc::ptr_eq(&self.context, &captured.context)
            && startup_shutdown::same_target(&self.failed_close, &captured.failed_close)
    }
}

#[cfg(test)]
impl Startup {
    pub(in crate::host) fn matches_for_test(
        &self,
        source: &Session,
        requested: &Session,
        context: &Arc<chat::protocol::ExecutionContext>,
    ) -> bool {
        &self.source == source
            && &self.requested == requested
            && Arc::ptr_eq(&self.context, context)
            && self.failed_close.is_none()
    }
}

/// Unit-only HostState fixtures intentionally exercise local transports without
/// a daemon lease. The retained-actor integration fixture opts into the genuine
/// exec/socket/lease boundary. Production uses the matched public protocol.
pub(super) fn enabled(provider: Provider, transport: &chat::protocol::TransportSpec) -> bool {
    #[cfg(test)]
    let enabled = std::env::var_os("CAGENTS_RETAINED_ACTOR_FIXTURE").is_some();
    #[cfg(not(test))]
    let enabled = true;
    enabled
        && provider == Provider::Claude
        && matches!(transport, chat::protocol::TransportSpec::JsonLines)
}

#[derive(Clone)]
pub(in crate::host) struct Broker(Arc<Inbox>);
struct Inbox {
    pending: Mutex<BTreeSet<TerminalId>>,
    wake: Condvar,
}

pub(super) fn wake(host: &SharedHost, id: TerminalId) {
    let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
    if state.shutting_down {
        return;
    }
    let broker = match state.native_actor_outcomes.clone() {
        Some(broker) => broker,
        None => {
            let inbox = Arc::new(Inbox {
                pending: Mutex::new(BTreeSet::new()),
                wake: Condvar::new(),
            });
            let weak = Arc::downgrade(host);
            let worker = inbox.clone();
            if let Err(error) = thread::Builder::new()
                .name("cagents-actor-outcomes".into())
                .spawn(move || run(weak, worker))
            {
                tracing::error!(%error, "cannot start retained actor outcome broker; helper outbox remains unacknowledged");
                return;
            }
            let broker = Broker(inbox);
            state.native_actor_outcomes = Some(broker.clone());
            broker
        }
    };
    drop(state);
    broker
        .0
        .pending
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(id);
    broker.0.wake.notify_one();
}

fn run(host: Weak<HostLock>, inbox: Arc<Inbox>) {
    let mut retry = BTreeSet::new();
    let mut warned = BTreeSet::new();
    loop {
        let mut pending = inbox.pending.lock().unwrap_or_else(|p| p.into_inner());
        if pending.is_empty() {
            pending = inbox
                .wake
                .wait_timeout(
                    pending,
                    if retry.is_empty() {
                        Duration::from_secs(5)
                    } else {
                        Duration::from_secs(1)
                    },
                )
                .unwrap_or_else(|p| p.into_inner())
                .0;
        }
        let mut work = std::mem::take(&mut *pending);
        drop(pending);
        work.append(&mut retry);
        let Some(host) = host.upgrade() else {
            return;
        };
        if host.lock().unwrap_or_else(|p| p.into_inner()).shutting_down {
            return;
        }
        for id in work {
            match drain(&host, id) {
                Ok(more) => {
                    warned.remove(&id);
                    if more {
                        retry.insert(id);
                    }
                }
                Err(error) => {
                    if warned.insert(id) {
                        tracing::warn!(%id, %error, "retained actor outcome is not committed; helper retains it for retry");
                    }
                    retry.insert(id);
                }
            }
        }
    }
}

/// An exact durable registration recognized by this helper attachment. The
/// census can predate an acknowledged console or daemon-proof projection; both
/// descriptors must be actual issued aliases, never inferred normalizations.
struct ExecutionOwner(super::super::model::NativeActorOwnership);

impl ExecutionOwner {
    fn authenticate(
        owner: Option<super::super::model::NativeActorOwnership>,
        identity: &chat::runtime::ActorIdentity,
        source: &Session,
        scope: Result<&crate::account::scope::LaunchAccountScope, &str>,
        recognize: impl FnOnce(&[Session]) -> Result<(), String>,
    ) -> Result<Self, String> {
        let owner = owner.ok_or("Retained execution lost its durable ownership registration")?;
        if owner.is_pending() {
            return Err("Pending cleanup intent cannot commit execution or affected-run loss; evidence remains unacknowledged".into());
        }
        if owner.actor() != identity.instance
            || owner.owner() != &identity.record
            || owner.scope() != scope
        {
            return Err("Retained execution changed its registered actor or captured account; evidence remains unacknowledged".into());
        }
        recognize(&[owner.source().clone(), source.clone()])?;
        Ok(Self(owner))
    }
}

/// Acquire epoch-authenticated source recognition once per drain, outside both
/// HOST and Registry locks. Non-run attention and zero-run loss remain usable
/// by cleanup-only owners and do not acquire execution authority.
fn capture_execution_owner<'a>(
    registry: &super::super::agent_execution::SharedRegistry,
    handle: &Handle,
    identity: &chat::runtime::ActorIdentity,
    source: &Session,
    execution: bool,
    captured: &'a mut Option<ExecutionOwner>,
) -> Result<Option<&'a ExecutionOwner>, String> {
    if !execution {
        return Ok(None);
    }
    if captured.is_none() {
        let owner = registry
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .native_actor_ownership(identity.instance)?;
        *captured = Some(ExecutionOwner::authenticate(
            owner,
            identity,
            source,
            handle.launch_account_scope(),
            |sources| handle.recognize_retained_sources(sources),
        )?);
    }
    Ok(captured.as_ref())
}

/// Recheck the exact captured row under the same Registry mutex as settlement.
/// Revocation, mutation or retirement after helper recognition cannot authorize
/// a durable run transition or the helper ACK that follows it.
fn require_execution_owner(
    registry: &super::super::model::Registry,
    captured: Option<&ExecutionOwner>,
    execution: bool,
) -> Result<(), String> {
    if !execution {
        return Ok(());
    }
    let captured = captured.ok_or("Retained execution lacks authenticated source ownership")?;
    let owner = registry
        .native_actor_ownership(captured.0.actor())?
        .ok_or("Retained execution lost its durable ownership registration")?;
    if owner != captured.0 || owner.is_pending() {
        return Err("Retained execution ownership changed after source authentication; evidence remains unacknowledged".into());
    }
    if registry.native_actor_retirement(owner.actor())?.is_some() {
        return Err("A retired actor cannot commit execution or affected-run loss; evidence remains unacknowledged".into());
    }
    Ok(())
}

#[cfg(test)]
#[path = "retained_execution_owner_tests.rs"]
mod execution_owner_tests;

struct SemanticTarget {
    handle: Handle,
    source: Session,
    identity: chat::runtime::ActorIdentity,
    notifications: Option<super::super::notifications::Handle>,
}

impl SemanticTarget {
    fn require_current(&self, host: &SharedHost) -> Result<(), String> {
        let current = host
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .chats
            .get(&self.source.id)
            .is_some_and(|record| {
                record.handle.same_attachment(&self.handle)
                    && record.session == self.source
                    && record.execution_instance == self.identity.instance
            });
        if current {
            Ok(())
        } else {
            Err("Retained actor ownership changed before semantic acknowledgement".into())
        }
    }
}

/// An Answer's input receipt proves adapter acceptance, not a committed HOST
/// run transition. Capture authority before sending it, then synchronize only
/// through the helper's post-acknowledgement semantic watermark.
pub(in crate::host) struct AnswerSync {
    target: SemanticTarget,
    registry: super::super::agent_execution::SharedRegistry,
    owner: Option<ExecutionOwner>,
}

impl AnswerSync {
    pub(in crate::host) fn capture(
        host: &SharedHost,
        handle: &Handle,
    ) -> Result<Option<Self>, String> {
        let Some(identity) = handle.actor_identity() else {
            return Ok(None);
        };
        let target = {
            let state = host.lock().unwrap_or_else(|p| p.into_inner());
            let record = state
                .chats
                .get(&identity.record.terminal)
                .filter(|record| {
                    record.handle.same_attachment(handle)
                        && record.execution_instance == identity.instance
                })
                .ok_or("The exact retained Answer attachment changed")?;
            SemanticTarget {
                handle: handle.clone(),
                source: record.session.clone(),
                identity,
                notifications: state.notifications.clone(),
            }
        };
        let registry = super::super::agent_execution::registry(host)?;
        let deadline = Instant::now() + Duration::from_secs(10);
        let registered = registry
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .native_actor_ownership(target.identity.instance)?;
        let owner = ExecutionOwner::authenticate(
            registered,
            &target.identity,
            &target.source,
            target.handle.launch_account_scope(),
            |sources| {
                target
                    .handle
                    .recognize_retained_sources_until(sources, deadline)
            },
        )?;
        let captured = Self {
            target,
            registry,
            owner: Some(owner),
        };
        captured.require_current(host)?;
        Ok(Some(captured))
    }

    fn require_current(&self, host: &SharedHost) -> Result<(), String> {
        self.target.require_current(host)?;
        require_execution_owner(
            &self.registry.lock().unwrap_or_else(|p| p.into_inner()),
            self.owner.as_ref(),
            true,
        )
    }

    fn require_committed_through(&self, through: u64) -> Result<(), String> {
        let registry = self.registry.lock().unwrap_or_else(|p| p.into_inner());
        require_execution_owner(&registry, self.owner.as_ref(), true)?;
        let committed = registry.native_actor_committed_through(
            self.target.identity.instance,
            &self.target.identity.record,
        )?;
        if committed < through {
            return Err(
                "The helper Answer acknowledgement exceeds the exact durable HOST semantic cursor"
                    .into(),
            );
        }
        Ok(())
    }

    pub(in crate::host) fn complete(mut self, host: &SharedHost) -> Result<(), String> {
        let deadline = Instant::now() + Duration::from_secs(10);
        self.require_current(host)?;
        let (mut acknowledged, through) =
            self.target.handle.retained_outcome_clock_until(deadline)?;
        // The journal admits at most 4096 entries; a byte-limited page may
        // contain just one. The deadline also bounds every RPC and receipt wait.
        for _ in 0..=4096 {
            self.require_current(host)?;
            if acknowledged >= through {
                return self.require_committed_through(through);
            }
            if Instant::now() >= deadline {
                break;
            }
            let page = self.target.handle.uncommitted_outcomes_until(deadline)?;
            for entry in page
                .into_iter()
                .take_while(|entry| entry.sequence <= through)
            {
                self.require_current(host)?;
                commit_semantic_entry(
                    host,
                    &self.target,
                    &self.registry,
                    &mut self.owner,
                    (entry.sequence, entry.update),
                    false,
                    deadline,
                )?;
            }
            let (next, _) = self.target.handle.retained_outcome_clock_until(deadline)?;
            if next <= acknowledged {
                return Err("Retained Answer semantic watermark made no committed progress".into());
            }
            acknowledged = next;
        }
        Err("Retained Answer semantic watermark remains uncommitted".into())
    }
}

fn drain(host: &SharedHost, id: TerminalId) -> Result<bool, String> {
    let (handle, source, identity, notifications) = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        let Some(record) = state.chats.get(&id) else {
            return Ok(false);
        };
        let Some(identity) = record.handle.actor_identity() else {
            return Ok(false);
        };
        (
            record.handle.clone(),
            record.session.clone(),
            identity,
            state.notifications.clone(),
        )
    };
    let registry = super::super::agent_execution::registry(host)?;
    let mut execution_owner = None;
    handle.refresh_retained_state()?;
    let loss = handle.continuity_loss();
    if let Some(loss) = &loss {
        let execution = !loss.affected_runs.is_empty();
        let owner = capture_execution_owner(
            &registry,
            &handle,
            &identity,
            &source,
            execution,
            &mut execution_owner,
        )?;
        let changed = {
            let mut registry = registry.lock().unwrap_or_else(|p| p.into_inner());
            require_execution_owner(&registry, owner, execution)?;
            registry.commit_native_actor_loss(
                identity.instance,
                &identity.record,
                &source,
                &loss.affected_runs,
                &loss.reason,
                chat::now_ms().unwrap_or(0),
            )?
        };
        if changed {
            super::super::agent_execution::announce(host, &registry);
        }
        let current = host
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .chats
            .get(&id)
            .is_some_and(|record| {
                record.handle.same_attachment(&handle)
                    && record.execution_instance == identity.instance
                    && record.session == source
            });
        if !current {
            return Err("Retained actor changed before continuity-loss acknowledgement".into());
        }
        handle.ack_continuity_loss(loss.id)?;
    }
    // Always ask the HELPER for its unacknowledged page. The local cursor alone
    // cannot prove that a notification committed before a previous daemon died.
    let page = handle.uncommitted_outcomes()?;
    let more = !page.is_empty();
    let target = SemanticTarget {
        handle,
        source,
        identity,
        notifications,
    };
    let deadline = Instant::now() + Duration::from_secs(30);
    for entry in page {
        commit_semantic_entry(
            host,
            &target,
            &registry,
            &mut execution_owner,
            (entry.sequence, entry.update),
            loss.is_some(),
            deadline,
        )?;
    }
    // One bounded page per actor per pass. A detached actor can have many
    // pages without another latest-sequence edge; explicitly revisit it until
    // an empty page confirms the helper's entire outbox is acknowledged.
    Ok(more)
}

/// The broker and synchronous Answer boundary share the same semantic writes,
/// exact owner checks, notification receipts and commit-before-ACK ordering.
fn commit_semantic_entry(
    host: &SharedHost,
    target: &SemanticTarget,
    registry: &super::super::agent_execution::SharedRegistry,
    execution_owner: &mut Option<ExecutionOwner>,
    entry: (u64, Update),
    lost: bool,
    deadline: Instant,
) -> Result<(), String> {
    if Instant::now() >= deadline {
        return Err("Retained semantic acknowledgement deadline expired".into());
    }
    target.require_current(host)?;
    let (sequence, update) = entry;
    let semantic = match update {
        Update::Execution { run, update } => NativeActorEvent::Execution { run, update },
        Update::Attention(evidence) => NativeActorEvent::Attention(evidence),
        Update::AttentionContext { evidence, content } => {
            NativeActorEvent::AttentionContext { evidence, content }
        }
        _ => return Err("The retained semantic outbox contains an unsupported event".into()),
    };
    let execution = matches!(semantic, NativeActorEvent::Execution { .. });
    capture_execution_owner(
        registry,
        &target.handle,
        &target.identity,
        &target.source,
        execution,
        execution_owner,
    )?;
    let authenticated = execution || execution_owner.is_some();
    let (changed, automation) = {
        let mut registry = registry.lock().unwrap_or_else(|p| p.into_inner());
        require_execution_owner(&registry, execution_owner.as_ref(), authenticated)?;
        let changed = registry.commit_native_actor_event(
            target.identity.instance,
            &target.identity.record,
            &target.source,
            sequence,
            &semantic,
            chat::now_ms().unwrap_or(0),
        )?;
        let automation = registry
            .agent_for_session(target.source.id)?
            .is_some_and(|agent| {
                matches!(
                    agent.origin,
                    crate::agent_execution::Origin::Automation { .. }
                )
            });
        (changed, automation)
    };
    if changed {
        super::super::agent_execution::announce(host, registry);
    }
    if let Some((evidence, content)) = semantic.attention()
        && !automation
        && !(lost && evidence.kind == crate::notifications::Kind::Succeeded)
    {
        let mut event = crate::notifications::Event::session(
            target.source.id,
            target.source.launch.provider,
            target.source.launch.recipe.working_directory.clone(),
            evidence,
        );
        event.content = content;
        let receipt = target
            .notifications
            .as_ref()
            .ok_or("The notification journal is unavailable; attention remains retained")?
            .record_committed(event)?;
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or("Retained semantic acknowledgement deadline expired")?;
        receipt
            .recv_timeout(remaining.min(Duration::from_secs(10)))
            .map_err(|_| "The notification journal has not confirmed this attention outcome")??;
    }
    target.require_current(host)?;
    require_execution_owner(
        &registry.lock().unwrap_or_else(|p| p.into_inner()),
        execution_owner.as_ref(),
        authenticated,
    )?;
    target
        .handle
        .commit_retained_outcomes_until(sequence, deadline)
}

/// Keep the exact cleanup owner even when helper spawn/installation replies
/// fail. A later retry must prove this owner's teardown before any new launch.
pub(super) fn forget_startup(
    host: &SharedHost,
    source: &Session,
    context: &Arc<chat::protocol::ExecutionContext>,
) {
    let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
    if !state.shutting_down
        && state
            .native_actor_startups
            .get(&source.id)
            .is_some_and(|pending| {
                &pending.source == source && Arc::ptr_eq(&pending.context, context)
            })
    {
        state.native_actor_startups.remove(&source.id);
        state.inventory_generation += 1;
    }
}

pub(super) fn reconcile_startup(host: &SharedHost, source: &Session) -> Result<bool, String> {
    if prepare_failed_start(host, source)?.is_some() {
        return Ok(true);
    }
    let pending = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        let Some(pending) = state.native_actor_startups.get(&source.id) else {
            return Ok(false);
        };
        if state.chats.contains_key(&source.id)
            || state.chat_spawning.contains_key(&source.id)
            || (&pending.source != source && &pending.requested != source)
        {
            return Err("The retained startup belongs to another exact descriptor or is still transitioning".into());
        }
        pending.clone()
    };
    if matches!(
        pending.context.ownership_snapshot().kind,
        chat::protocol::LaunchOwnerKind::RemoteActor { .. }
    ) {
        startup_shutdown::cleanup_normal_startup(host, &pending)?;
        forget_startup(host, &pending.source, &pending.context);
        return Ok(true);
    }
    match pending.context.reconcile_cleanup()? {
        chat::protocol::LaunchOutcome::NoProvider
        | chat::protocol::LaunchOutcome::TeardownProved => {
            forget_startup(host, &pending.source, &pending.context);
            Ok(true)
        }
        chat::protocol::LaunchOutcome::Uncertain => Err(
            "The exact retained startup has not proved cleanup; no second provider was started"
                .into(),
        ),
    }
}

/// Explicit retry may discover the still-owned failed attachment in ChatList.
/// Preparation finishes that exact normal Close and returns only its original
/// requested launch. An unrelated or successful retained actor never gains a
/// restart grant through this path.
pub(super) fn prepare_failed_start(
    host: &SharedHost,
    source: &Session,
) -> Result<Option<Session>, String> {
    let failed = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        state
            .native_actor_startups
            .get(&source.id)
            .and_then(|pending| {
                pending.failed_close.as_ref().and_then(|target| {
                    (source == &pending.requested || source == &target.source)
                        .then(|| (pending.clone(), target.clone()))
                })
            })
    };
    if let Some((pending, target)) = failed {
        if matches!(
            pending.context.ownership_snapshot().kind,
            chat::protocol::LaunchOwnerKind::RemoteActor { .. }
        ) {
            startup_shutdown::cleanup_normal_startup(host, &pending)?;
        } else {
            super::close_captured(host, target).map_err(|error| error.message)?;
        }
        forget_startup(host, &pending.source, &pending.context);
        return Ok(Some(pending.requested));
    }
    Ok(None)
}

/// Persist cleanup authority while the inspected source and captured scope are
/// still authenticated, before installation or a new attachment can retire the
/// previous epoch. Pending is never an input or history-resume grant.
fn enroll_pending(
    host: &SharedHost,
    identity: &chat::runtime::ActorIdentity,
    issued: &Session,
    aliases: &[Session],
    scope: Result<&crate::account::scope::LaunchAccountScope, &str>,
    installed: bool,
) -> Result<(), String> {
    let registry = super::super::agent_execution::registry(host)?;
    let mut registry = registry.lock().unwrap_or_else(|p| p.into_inner());
    let existing = registry.native_actor_ownership(identity.instance)?;
    let source = if let Some(existing) = &existing {
        if existing.owner() != &identity.record
            || (existing.source() != issued && !aliases.contains(existing.source()))
            || existing.scope() != scope
        {
            return Err(
                "Pending cleanup intent changed its exact helper, source or captured scope".into(),
            );
        }
        existing.source()
    } else {
        if installed {
            return Err("An installed retained actor has no prior durable ownership intent; unknown work was preserved".into());
        }
        issued
    };
    registry.register_pending_native_actor_ownership(
        identity.instance,
        &identity.record,
        source,
        scope,
    )
}

#[derive(Clone, Copy)]
pub(super) enum Publication {
    Install,
    Close,
}

/// Commit normal installation before projecting ownership, then publish one
/// exact acknowledged tuple to Registry and return that same tuple to the HOST.
/// Close-only attachment keeps its existing ownership and never installs input.
/// Every caller supplies previously authenticated/issued aliases and revalidates
/// exact actor/pair ownership before exposing the returned record.
pub(super) fn publish_authenticated_source(
    host: &SharedHost,
    handle: &Handle,
    mut source: Session,
    retained: &[crate::agent_execution::HostedSession],
    console: Option<crate::model::LaunchRecipe>,
    pair: Option<crate::terminal::holder::Record>,
    publication: Publication,
) -> Result<Session, String> {
    if handle.actor_identity().is_none() {
        return Err("Retained publication lost its authenticated actor identity".into());
    }
    let install = matches!(publication, Publication::Install);
    if install == handle.is_close_only() {
        return Err("Retained publication mode does not match its authenticated attachment".into());
    }
    if install {
        let proof = source
            .proof
            .clone()
            .ok_or("Pending publication lacks daemon proof")?;
        let identity = handle.actor_identity().unwrap();
        let inspection = Handle::inspect_remote(&identity.record, proof)?;
        if inspection.identity != identity {
            return Err("Pending publication changed its exact authenticated actor".into());
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
        handle.commit_native_installation()?;
    }
    source.launch.session_id = handle.confirmed_session_id().or(source.launch.session_id);
    source.native_continuity = handle.native_continuity();
    if install
        && (!matches!(
            source.native_continuity,
            chat::NativeContinuity::RetainedClaudeActor { protocol: 2 }
        ) || !source.has_valid_native_continuity())
    {
        return Err("Installed Claude actor did not prove retained ownership".into());
    }
    handle.update_native_source(source.clone(), console, pair)?;
    if install {
        super::super::agent_execution::registry(host)?
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .adopt_authenticated_agent_session(
                retained,
                &crate::agent_execution::HostedSession::Chat(source.clone()),
            )?;
    }
    if install {
        // Promotion follows the exact helper ACK and Registry transaction and
        // precedes any ordinary HOST input record becoming visible.
        register_ownership(host, handle, &source)?;
    }
    Ok(source)
}

/// Reattach to the resident Claude reducer; never open provider stdin, reload
/// its transcript as a replacement, or reconstruct command/control UUIDs.
pub(super) fn restore(
    host: &SharedHost,
    client: ClientId,
    requested: &Session,
    proof: &HostSessionProof,
    owner: crate::daemon::holder_registry::Record,
) -> Result<ServerResponse, String> {
    restore_actor(host, client, requested, proof, owner, true, None, None)
}

/// A failed cleanup attachment is not a user request to return to a terminal.
/// Restore its still-running owner through normal installation, retaining the
/// old cleanup record until the helper and durable registry acknowledge it.
pub(super) fn recover_cleanup_attachment(
    host: &SharedHost,
    requested: &Session,
) -> Result<(), String> {
    super::super::account_lifecycle::ensure_idle(host).map_err(|error| error.message)?;
    let (target, identity, proof, _reservation) = {
        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
        let record = state
            .chats
            .get(&requested.id)
            .ok_or("Cleanup owner disappeared")?;
        if !record.handle.is_close_only()
            || record.session.launch.provider != Provider::Claude
            || record.handle.is_completed()
            || state.shutting_down
            || state.chat_handoffs.contains_key(&requested.id)
            || state.native_closing.contains_key(&requested.id)
        {
            return Err("This conversation is not a running recoverable cleanup attachment".into());
        }
        let target = super::CloseTarget::from_record(record);
        let identity = record
            .handle
            .actor_identity()
            .ok_or("Cleanup lost its exact helper")?;
        let proof = state.proof().clone();
        let reservation = capacity::Reservation::closing(host, &mut state, requested)
            .map_err(|error| error.message)?;
        (target, identity, proof, reservation)
    };
    restore_actor(
        host,
        0,
        requested,
        &proof,
        identity.record,
        true,
        Some(&target),
        None,
    )?;
    Ok(())
}

pub(in crate::host) fn recover_cleanup_attachments(host: &SharedHost) -> Vec<String> {
    let sources = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .chats
        .values()
        .filter(|record| record.handle.is_close_only())
        .map(|record| record.session.clone())
        .collect::<Vec<_>>();
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut failures = Vec::new();
    for source in sources {
        if Instant::now() >= deadline {
            failures.push("Conversation recovery reached its preview deadline; existing ownership was preserved.".into());
            break;
        }
        if recover_cleanup_attachment(host, &source).is_err() && failures.len() < 40 {
            failures.push(format!("Conversation {} could not reconnect after an unfinished close; its provider and recovery evidence were preserved.", source.id));
        }
    }
    failures
}

#[cfg(test)]
pub(super) fn restore_cleanup_for_test(
    host: &SharedHost,
    requested: &Session,
    proof: &HostSessionProof,
    owner: crate::daemon::holder_registry::Record,
) -> Result<ServerResponse, String> {
    restore_actor(host, 0, requested, proof, owner, false, None, None)
}

fn restore_actor(
    host: &SharedHost,
    client: ClientId,
    requested: &Session,
    proof: &HostSessionProof,
    owner: crate::daemon::holder_registry::Record,
    publish_snapshot: bool,
    replacing: Option<&super::CloseTarget>,
    shutdown: Option<&startup_shutdown::Capture>,
) -> Result<ServerResponse, String> {
    if let Some(capture) = shutdown {
        capture.require_reserved_host(host)?;
        capture.require_live_adoption()?;
    }
    let inspection = Handle::inspect_remote(&owner, proof.clone())?;
    if inspection.identity.record != owner
        || inspection.source.id != requested.id
        || inspection.source.launch.provider != Provider::Claude
    {
        return Err("The retained Claude actor identity changed during inspection".into());
    }
    if let Some(target) = replacing {
        target
            .require_current(host)
            .map_err(|error| error.message)?;
        if target.handle.actor_identity().as_ref() != Some(&inspection.identity)
            || !inspection.state.installed
            || inspection.state.completed
            || !inspection.state.owns_work
            || inspection.state.close.closing
            || inspection.state.close.exited
        {
            return Err("The cleanup helper no longer owns a running provider; retirement evidence was preserved".into());
        }
    }
    if let Some(capture) = shutdown {
        capture.require_inspection(
            &inspection.identity,
            &inspection.source,
            &inspection.previous_sources,
        )?;
        capture.enroll_uninstalled(
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
    if !publish_snapshot {
        verify_close_ownership(
            host,
            &inspection.identity,
            &inspection.source,
            &inspection.previous_sources,
            inspection
                .launch_account_scope
                .as_ref()
                .map_err(String::as_str),
        )?;
    } else {
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
    let weak = Arc::downgrade(host);
    let id = requested.id;
    let expected_instance = inspection.identity.instance;
    let notify = move |update| changed_owned(&weak, id, expected_instance, update);
    let handle = if publish_snapshot {
        Handle::adopt_remote(&owner, requested.clone(), proof.clone(), notify)?
    } else {
        Handle::adopt_remote_for_close(&owner, requested.clone(), proof.clone(), notify)?
    };
    let identity = handle
        .actor_identity()
        .ok_or("The retained actor did not provide its stable identity")?;
    if identity != inspection.identity {
        return Err("The retained actor changed while attaching".into());
    }
    let previous = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        let current = state.chats.get(&id).and_then(|record| {
            (record.handle.actor_identity().as_ref() == Some(&identity))
                .then(|| record.handle.clone())
        });
        let startup = state
            .native_actor_startups
            .get(&id)
            .and_then(|pending| pending.context.ownership_snapshot().remote)
            .filter(|handle| handle.actor_identity().as_ref() == Some(&identity));
        [current, startup]
    };
    for previous in previous.into_iter().flatten() {
        handle.inherit_original_helper_from(&previous)?;
    }
    if let Some(capture) = shutdown {
        capture.inherit_original_helper(&handle)?;
    }
    let mut session = requested.clone();
    session.proof = Some(proof.clone());
    // A presentation alias predating the console cannot release its current
    // exact paired worker. Only authenticated helper metadata names that pair.
    session.terminal = inspection.source.terminal.clone();
    let paired = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        match (&mut session.terminal, inspection.paired_holder.as_ref()) {
            (None, None) => None,
            // The issued descriptor can name a historical terminal during a
            // conversion startup, before its explicit unpair ACK. Cleanup may
            // discard that view only when authenticated helper metadata proves
            // it owns neither pair nor console; no terminal worker is touched.
            (Some(_), None) if !publish_snapshot && inspection.execution_console.is_none() => {
                session.terminal = None;
                None
            }
            (Some(terminal), Some(owner)) => {
                let current = state.terminals.get(&terminal.id).filter(|current|
                    current.handle.holder_identity() == Some(owner)
                    && current.session.launch == terminal.launch
                    && current.session.owned_codex_listener == terminal.owned_codex_listener)
                    .ok_or("The retained Claude console's exact worker could not be recovered; provider ownership was preserved")?;
                *terminal = state.native_terminal_projection(terminal.id)?;
                if terminal.continuity.live_proof().as_ref() != Some(proof) {
                    return Err(
                        "The retained Claude console belongs to a different daemon owner".into(),
                    );
                }
                Some(current.handle.clone())
            }
            _ => {
                return Err(
                    "The retained Claude pair lacks exact worker evidence; no pair was released"
                        .into(),
                );
            }
        }
    };
    let mut sources = inspection.previous_sources;
    sources.push(inspection.source);
    sources.push(requested.clone());
    sources.sort_by_key(|source| serde_json::to_vec(source).unwrap_or_default());
    sources.dedup();
    session = publish_authenticated_source(
        host,
        &handle,
        session,
        &sources
            .iter()
            .cloned()
            .map(crate::agent_execution::HostedSession::Chat)
            .collect::<Vec<_>>(),
        inspection.execution_console.clone(),
        inspection.paired_holder.clone(),
        if publish_snapshot {
            Publication::Install
        } else {
            Publication::Close
        },
    )?;
    let after_generation = handle.observe_next_update();
    let observation = Observation {
        after_generation,
        ..Observation::default()
    };
    let previous_sources = super::native_recovery::retained_upgrade_sources(sources, &session);
    let mut record = Record {
        execution_instance: identity.instance,
        console_recipe: inspection.execution_console,
        paired_terminal: paired.clone(),
        session,
        handle: handle.clone(),
        source_process: None,
        subscribers: if client == 0 {
            BTreeMap::new()
        } else {
            BTreeMap::from([(client, false)])
        },
        observations: if client == 0 {
            BTreeMap::new()
        } else {
            BTreeMap::from([(client, observation)])
        },
        completion_observed: false,
        returning_terminal: !publish_snapshot,
    };
    let response = if publish_snapshot {
        session_snapshot(&record.session, &record.handle)?
    } else {
        // Cleanup adopts ownership without reading a presentation window.
        ServerResponse::Accepted
    };
    let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
    let pair_unchanged = match (&record.session.terminal, &paired) {
        (None, None) => true,
        (Some(terminal), Some(handle)) => {
            state.terminals.get(&terminal.id).is_some_and(|current| {
                current.handle.same_worker(handle)
                    && state.native_terminal_projection(terminal.id).as_ref() == Ok(terminal)
            })
        }
        _ => false,
    };
    if shutdown.is_some_and(|capture| capture.require_reserved_state(&state).is_err())
        || (state.shutting_down && shutdown.is_none())
        || state.proof() != proof
        || !pair_unchanged
        || match replacing {
            Some(target) => state
                .chats
                .get(&id)
                .is_none_or(|current| !target.matches(current)),
            None => state.chats.contains_key(&id),
        }
        || state
            .chat_spawning
            .get(&id)
            .is_some_and(|reserved| reserved != requested)
    {
        return Err("Daemon ownership changed before retained Claude registration; the provider remains owned by its holder".into());
    }
    if replacing.is_some() {
        // Failed legacy attachments may already have observers in another
        // window. Preserve those authorized subscriptions, rebasing their
        // notification fence onto the actual new attachment's generation.
        let previous = state.chats.get(&id).expect("replacement was validated");
        record.subscribers.extend(previous.subscribers.clone());
        record.observations = record
            .subscribers
            .keys()
            .map(|&client| {
                (
                    client,
                    Observation {
                        epoch: previous
                            .observations
                            .get(&client)
                            .map_or(0, |observation| observation.epoch),
                        after_generation,
                        notified: false,
                    },
                )
            })
            .collect();
    }
    let visible = record.subscribers.values().any(|visible| *visible);
    let retired = state.chats.insert(id, record);
    state.upgrade_chats.insert(id, previous_sources);
    state.native_adoption_failures.clear(id);
    state.inventory_generation += 1;
    drop(state);
    drop(retired);
    if visible {
        handle.set_visible(true);
    }
    handle.wake_retained_outcomes();
    super::super::state::retire_unattached_upgrade_chat(host, id);
    Ok(response)
}

/// Closure uses the originally captured tuple, not permission to reopen that
/// home. Require the durable actor registration before a close-only attachment.
fn verify_close_ownership(
    host: &SharedHost,
    identity: &chat::runtime::ActorIdentity,
    source: &Session,
    aliases: &[Session],
    scope: Result<&crate::account::scope::LaunchAccountScope, &str>,
) -> Result<bool, String> {
    let registry = super::super::agent_execution::registry(host)?;
    let mut after = None;
    loop {
        let page = registry
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .native_actor_ownerships(after, 128)?;
        if page.is_empty() {
            return Err(
                "The retained actor has no authenticated durable ownership registration".into(),
            );
        }
        for ownership in page {
            after = Some(ownership.actor());
            if ownership.actor() != identity.instance {
                continue;
            }
            if ownership.owner() != &identity.record
                || (ownership.source() != source && !aliases.contains(ownership.source()))
                || ownership.scope() != scope
            {
                return Err("Close-only attachment changed its registered actor, source or captured account".into());
            }
            return Ok(ownership.is_pending());
        }
    }
}

/// Explicit cleanup of an authenticated helper which has no client attachment.
/// Adoption retains the original reducer and pipes; this path never launches.
pub(super) fn close_unattached(
    host: &SharedHost,
    requested: &Session,
    discovered: Option<crate::daemon::holder_registry::Record>,
) -> Result<(), String> {
    close_with_target(host, requested, discovered, None, None, None, false, false)
}

/// A hosted close must retain its exact admission capability across cleanup.
pub(super) fn close_captured(
    host: &SharedHost,
    requested: &Session,
    target: &super::CloseTarget,
    closing: Option<&super::capacity::Reservation>,
) -> Result<(), String> {
    close_with_target(
        host,
        requested,
        None,
        Some(target),
        closing,
        None,
        false,
        false,
    )
}

fn close_with_target(
    host: &SharedHost,
    requested: &Session,
    discovered: Option<crate::daemon::holder_registry::Record>,
    expected: Option<&super::CloseTarget>,
    closing: Option<&super::capacity::Reservation>,
    shutdown: Option<&startup_shutdown::Capture>,
    retain_startup: bool,
    retired_only: bool,
) -> Result<(), String> {
    struct Reservation {
        host: SharedHost,
        source: Session,
    }
    impl Drop for Reservation {
        fn drop(&mut self) {
            let mut state = self.host.lock().unwrap_or_else(|p| p.into_inner());
            if state.chat_spawning.get(&self.source.id) == Some(&self.source) {
                state.chat_spawning.remove(&self.source.id);
                state.inventory_generation += 1;
            }
        }
    }
    let proof = {
        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
        if shutdown.is_some_and(|capture| capture.require_state(&state).is_err())
            || (state.shutting_down && shutdown.is_none())
            || closing.map_or_else(
                || state.chat_spawning.contains_key(&requested.id),
                |closing| !closing.holds(&state, requested.id),
            )
            || state.chat_handoffs.contains_key(&requested.id)
            || state.native_closing.contains_key(&requested.id)
            || (retired_only && state.chats.contains_key(&requested.id))
        {
            return Err("Native ownership changed or another exact transition is pending".into());
        }
        if let Some(expected) = expected
            && !state
                .chats
                .get(&requested.id)
                .is_some_and(|record| expected.matches(record))
        {
            return Err("The exact hosted attachment changed before retained cleanup".into());
        }
        if closing.is_none() {
            state.chat_spawning.insert(requested.id, requested.clone());
            state.inventory_generation += 1;
        }
        state.proof().clone()
    };
    let _reservation = closing.is_none().then(|| Reservation {
        host: host.clone(),
        source: requested.clone(),
    });
    let present = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .chats
        .contains_key(&requested.id);
    if !present {
        if let Some(verified) = cleanup_retirement(host, requested, &proof, discovered.as_ref())? {
            return close_verified_unattached(host, requested, &proof, verified, shutdown);
        }
        if retired_only {
            return Err("The captured failed target lacks exact durable verified retirement; live adoption was refused".into());
        }
        let owner = discovered.as_ref().ok_or("No authenticated retained Claude actor is available; its saved descriptor was preserved")?;
        if !Handle::is_remote_record(owner) {
            return Err(
                "The discovered native holder does not support this retained Claude protocol"
                    .into(),
            );
        }
        restore_actor(
            host,
            0,
            requested,
            &proof,
            owner.clone(),
            false,
            None,
            shutdown,
        )?;
    }
    if retired_only {
        return Err("A HOST attachment appeared during receipt-only failed-target cleanup".into());
    }
    let (handle, source, paired, target) = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        let record = state
            .chats
            .get(&requested.id)
            .ok_or("The retained actor changed during cleanup admission")?;
        if let Some(capture) = shutdown {
            capture.require_reserved_state(&state)?;
        }
        if expected.is_some_and(|expected| !expected.matches(record)) {
            return Err(
                "The exact hosted attachment changed during retained cleanup admission".into(),
            );
        }
        let identity = record
            .handle
            .actor_identity()
            .ok_or("This descriptor belongs to another native runtime")?;
        if discovered
            .as_ref()
            .is_some_and(|owner| *owner != identity.record)
        {
            return Err(
                "The discovered retained actor differs from the current exact owner".into(),
            );
        }
        let mut observed = record.session.clone();
        observed.launch.session_id = record.handle.session_id();
        observed.native_continuity = record.handle.native_continuity();
        if record.session != *requested
            && observed != *requested
            && !state
                .upgrade_chats
                .get(&requested.id)
                .is_some_and(|aliases| aliases.iter().any(|alias| alias.source == *requested))
        {
            return Err(
                "The retained actor does not recognize this exact cleanup descriptor".into(),
            );
        }
        let paired = match (&record.session.terminal, &record.paired_terminal) {
            (None, None) => None,
            (Some(terminal), Some(expected)) => {
                let current = state.terminals.get(&terminal.id)
                    .filter(|current| current.handle.same_worker(expected))
                    .ok_or("The original paired worker is unavailable or replaced; no replacement was stopped")?;
                let observed = if current.handle.is_completed() {
                    // A previous attempt may have closed this exact worker
                    // before the final registry transaction failed. Its issued
                    // fields remain fixed; close_with_receipt below must still
                    // authenticate the original holder's actual teardown.
                    let mut observed = current.session.clone();
                    observed.continuity = terminal.continuity.clone();
                    observed
                } else {
                    state.native_terminal_projection(terminal.id)?
                };
                if observed != *terminal {
                    return Err(
                        "The paired terminal descriptor changed; its replacement was preserved"
                            .into(),
                    );
                }
                Some((terminal.clone(), current.handle.clone()))
            }
            _ => return Err("Retained Claude pairing lacks its exact worker evidence".into()),
        };
        (
            record.handle.clone(),
            record.session.clone(),
            paired,
            super::CloseTarget::from_record(record),
        )
    };
    super::super::agent_execution::preflight_retirement(host, &source)?;
    // Authenticate all managed execution aliases before a destructive effect.
    let registry = super::super::agent_execution::registry(host)?;
    let sources = registry
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .retained_agent_sources(source.id)?;
    let identity = handle
        .actor_identity()
        .ok_or("Retained cleanup lost its actor identity")?;
    let (issued, lineage, scope, retired) =
        match Handle::inspect_retained_retirement(&identity.record, &proof)? {
            RetirementInspection::Running => {
                let inspection = Handle::inspect_remote(&identity.record, proof.clone())?;
                if inspection.identity != identity {
                    return Err("The retained cleanup actor changed during inspection".into());
                }
                (
                    inspection.source,
                    inspection.previous_sources,
                    inspection.launch_account_scope,
                    None,
                )
            }
            RetirementInspection::Verified(verified) => {
                let receipt = verified.receipt();
                if receipt.identity != identity {
                    return Err("The retained cleanup receipt belongs to another actor".into());
                }
                (
                    receipt.source.clone(),
                    receipt.previous_sources.clone(),
                    receipt.launch_account_scope.clone(),
                    Some(verified),
                )
            }
            RetirementInspection::ExitedUnproved => {
                return Err("The closed helper lacks its exact semantic-retirement receipt".into());
            }
        };
    if let Some(capture) = shutdown {
        capture.require_reserved_host(host)?;
        capture.require_inspection(&identity, &issued, &lineage)?;
    }
    verify_close_ownership(
        host,
        &identity,
        &issued,
        &lineage,
        scope.as_ref().map_err(String::as_str),
    )?;
    for retained in &sources {
        let crate::agent_execution::HostedSession::Chat(current) = retained else {
            return Err("Retained execution ownership changed to another surface".into());
        };
        if issued != *current && !lineage.contains(current) {
            return Err(
                "The current execution descriptor is outside this exact retained actor lineage"
                    .into(),
            );
        }
        registry
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .preflight_agent_retirement(retained)?;
    }
    target
        .require_current_with_shutdown(host, shutdown)
        .map_err(|error| error.message)?;
    if retired.is_none() {
        handle.close()?;
    }
    target
        .require_current_with_shutdown(host, shutdown)
        .map_err(|error| error.message)?;
    if let Some((terminal, worker)) = &paired {
        let current = {
            let state = host.lock().unwrap_or_else(|p| p.into_inner());
            state.terminals.get(&terminal.id).is_some_and(|current| {
                current.handle.same_worker(worker)
                    && super::matches_paired_terminal(&current.session, terminal, state.proof())
            })
        };
        if !current {
            return Err(
                "The paired worker changed before retained cleanup; replacement was preserved"
                    .into(),
            );
        }
        worker.close_with_receipt(terminal, &proof)?;
    }
    target
        .require_current_with_shutdown(host, shutdown)
        .map_err(|error| error.message)?;
    if let Some(verified) = retired {
        publish_verified_retirement(host, &handle, &source, verified)?;
    } else {
        finish_close(host, &handle, &source)?;
    }
    target
        .require_current_with_shutdown(host, shutdown)
        .map_err(|error| error.message)?;
    // Keep the closed record retryable until durable process retirement succeeds.
    super::super::agent_execution::retired(host, &source)?;
    let normal_startup = if !retain_startup && shutdown.is_none() {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        state
            .native_actor_startups
            .get(&source.id)
            .filter(|pending| {
                pending.failed_close.as_ref().is_some_and(|failed| {
                    state
                        .chats
                        .get(&source.id)
                        .is_some_and(|record| failed.matches(record))
                })
            })
            .cloned()
    } else {
        None
    };
    if let Some(pending) = &normal_startup {
        let snapshot = pending.context.ownership_snapshot();
        if let chat::protocol::LaunchOwnerKind::RemoteActor { identity } = snapshot.kind {
            if handle.actor_identity().as_ref() != Some(&identity)
                || !snapshot.token.is_some_and(|token| {
                    pending.context.complete_semantic_cleanup(token, &identity)
                })
            {
                return Err(
                    "Semantic close committed; the original startup helper wait remains pending"
                        .into(),
                );
            }
        }
    }
    let removed = {
        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
        if shutdown.is_some_and(|capture| capture.require_reserved_state(&state).is_err())
            || (state.shutting_down && shutdown.is_none())
            || state.proof() != &proof
            || !state
                .chats
                .get(&source.id)
                .is_some_and(|record| target.matches(record))
            || paired.as_ref().is_some_and(|(terminal, worker)| {
                state
                    .terminals
                    .get(&terminal.id)
                    .is_none_or(|current| !current.handle.same_worker(worker))
            })
            || normal_startup.as_ref().is_some_and(|captured| {
                state
                    .native_actor_startups
                    .get(&source.id)
                    .is_none_or(|current| !current.matches(captured))
            })
        {
            return Err(
                "Retained ownership changed during cleanup; replacement records were preserved"
                    .into(),
            );
        }
        if let Some((terminal, _)) = &paired {
            state.retire_terminal(terminal.id);
        }
        let removed = state.chats.remove(&source.id);
        // Ordinary Close may finish a previously failed initialization too.
        // Clear only the startup captured by this fully retired attachment;
        // a replacement owner or an unproved teardown retains its reservation.
        if !retain_startup
            && !state.shutting_down
            && removed.as_ref().is_some_and(|record| {
                state
                    .native_actor_startups
                    .get(&source.id)
                    .is_some_and(|pending| {
                        pending
                            .failed_close
                            .as_ref()
                            .is_some_and(|failed| failed.matches(record))
                    })
            })
        {
            state.native_actor_startups.remove(&source.id);
        }
        state.native_adoption_failures.clear(source.id);
        if let Some(notifications) = &state.notifications {
            notifications.retire(source.id);
        }
        state.inventory_generation += 1;
        removed
    };
    drop(removed);
    super::super::state::retire_unattached_upgrade_chat(host, source.id);
    Ok(())
}

/// Discovery disappears at retirement, before the durable HOST transaction.
/// Consult only this pane's durable owners; absence never authorizes relaunch.
fn cleanup_retirement(
    host: &SharedHost,
    requested: &Session,
    proof: &HostSessionProof,
    discovered: Option<&crate::daemon::holder_registry::Record>,
) -> Result<Option<VerifiedRetirement>, String> {
    let registry = super::super::agent_execution::registry(host)?;
    let mut after = None;
    let mut matched = None;
    loop {
        let page = registry
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .native_actor_ownerships(after, 128)?;
        if page.is_empty() {
            return Ok(matched);
        }
        for owner in page {
            after = Some(owner.actor());
            if owner.source().id != requested.id
                || discovered.is_some_and(|record| record != owner.owner())
            {
                continue;
            }
            match Handle::inspect_retained_retirement(owner.owner(), proof)? {
                RetirementInspection::Running => {}
                RetirementInspection::ExitedUnproved => {
                    return Err(
                        "The retained cleanup helper exited without verified retirement".into(),
                    );
                }
                RetirementInspection::Verified(verified) => {
                    let receipt = verified.receipt();
                    if receipt.identity.instance != owner.actor()
                        || &receipt.identity.record != owner.owner()
                        || (&receipt.source != requested
                            && !receipt.previous_sources.contains(requested))
                    {
                        return Err(
                            "The retired actor does not recognize this exact cleanup descriptor"
                                .into(),
                        );
                    }
                    if matched.replace(verified).is_some() {
                        return Err(
                            "The cleanup descriptor matches more than one retired actor".into()
                        );
                    }
                }
            }
        }
    }
}

/// A verified capsule grants no live attachment, input or account access.
/// Its paired tuple still needs the terminal holder's independent close proof.
fn close_verified_unattached(
    host: &SharedHost,
    requested: &Session,
    proof: &HostSessionProof,
    verified: VerifiedRetirement,
    shutdown: Option<&startup_shutdown::Capture>,
) -> Result<(), String> {
    let receipt = verified.receipt();
    if let Some(capture) = shutdown {
        capture.require_reserved_host(host)?;
        capture.require_inspection(
            &receipt.identity,
            &receipt.source,
            &receipt.previous_sources,
        )?;
    }
    verify_close_ownership(
        host,
        &receipt.identity,
        &receipt.source,
        &receipt.previous_sources,
        receipt
            .launch_account_scope
            .as_ref()
            .map_err(String::as_str),
    )?;
    match (&receipt.source.terminal, &receipt.paired_holder) {
        (None, None) => {}
        (Some(terminal), Some(pair))
            if terminal.id == pair.terminal && pair.terminal == requested.id =>
        {
            let role = crate::daemon::holder_registry::OriginalRole::authenticated(
                pair,
                &receipt.proof,
                proof,
            )?;
            if !crate::terminal::holder::cleanup::confirmed_owner_with_role(
                pair,
                proof,
                role.as_ref(),
            )? {
                return Err(
                    "The retired actor's exact paired terminal has no confirmed close proof".into(),
                );
            }
        }
        _ => return Err("Retired cleanup lost its exact original paired terminal evidence".into()),
    }
    let pair =
        {
            let state = host.lock().unwrap_or_else(|p| p.into_inner());
            if shutdown.is_some_and(|capture| capture.require_reserved_state(&state).is_err())
                || (state.shutting_down && shutdown.is_none())
                || state.proof() != proof
                || state.chats.contains_key(&requested.id)
                || state.chat_spawning.get(&requested.id) != Some(requested)
            {
                return Err("Daemon ownership changed before retired cleanup".into());
            }
            match (&receipt.source.terminal, state.terminals.get(&requested.id)) {
                (Some(terminal), Some(record))
                    if record.handle.is_completed()
                        && super::matches_paired_terminal(&record.session, terminal, proof) =>
                {
                    Some(record.handle.clone())
                }
                (_, None) => None,
                _ => return Err(
                    "The paired worker changed before retired cleanup; replacement was preserved"
                        .into(),
                ),
            }
        };
    let registry = super::super::agent_execution::registry(host)?;
    let aliases = registry
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .retained_agent_sources(requested.id)?;
    for retained in &aliases {
        let crate::agent_execution::HostedSession::Chat(source) = retained else {
            return Err("Retained cleanup execution changed to another surface".into());
        };
        if source != &receipt.source && !receipt.previous_sources.contains(source) {
            return Err("Retained cleanup execution is outside the exact issued lineage".into());
        }
        registry
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .preflight_agent_retirement(retained)?;
    }
    registry
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .commit_native_actor_retirement(
            receipt.identity.instance,
            &receipt.identity.record,
            &receipt.source,
            receipt
                .launch_account_scope
                .as_ref()
                .map_err(String::as_str),
            receipt.confirmed_session.as_deref(),
        )?;
    for retained in &aliases {
        if let crate::agent_execution::HostedSession::Chat(source) = retained {
            super::super::agent_execution::retired(host, source)?;
        }
    }
    {
        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
        if shutdown.is_some_and(|capture| capture.require_reserved_state(&state).is_err())
            || (state.shutting_down && shutdown.is_none())
            || state.proof() != proof
            || state.chats.contains_key(&requested.id)
            || state.chat_spawning.get(&requested.id) != Some(requested)
            || match (&pair, state.terminals.get(&requested.id)) {
                (None, None) => false,
                (Some(expected), Some(record)) => !record.handle.same_worker(expected),
                _ => true,
            }
        {
            return Err(
                "Retired cleanup ownership changed; replacement metadata was preserved".into(),
            );
        }
        if pair.is_some() {
            state.retire_terminal(requested.id);
        }
        state.native_adoption_failures.clear(requested.id);
        if let Some(notifications) = &state.notifications {
            notifications.retire(requested.id);
        }
        state.inventory_generation += 1;
    }
    super::super::state::retire_unattached_upgrade_chat(host, requested.id);
    Ok(())
}

/// These IDs came from the authenticated actor's journal at attachment, not
/// from a user pane, busy bit, or a prior daemon's accepted dispatch record.
pub(in crate::host) fn retained_runs(
    host: &SharedHost,
) -> Vec<(crate::model::RunId, crate::agent_execution::HostedSession)> {
    let actors = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        state
            .chats
            .values()
            .filter_map(|record| {
                let identity = record.handle.actor_identity()?;
                (identity.instance == record.execution_instance)
                    .then(|| (record.handle.clone(), record.session.clone()))
            })
            .collect::<Vec<_>>()
    };
    actors
        .into_iter()
        .flat_map(|(handle, source)| {
            handle.retained_runs().into_iter().map(move |run| {
                (
                    run,
                    crate::agent_execution::HostedSession::Chat(source.clone()),
                )
            })
        })
        .collect()
}

/// Boot must record a known loss before deciding which accepted runs remain
/// protected by the surviving actor. Ordinary deferred delivery still retries.
pub(in crate::host) fn reconcile_startup_losses(host: &SharedHost) -> Result<(), String> {
    let actors = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        state
            .chats
            .values()
            .filter_map(|record| {
                record
                    .handle
                    .actor_identity()
                    .map(|identity| (record.handle.clone(), record.session.clone(), identity))
            })
            .collect::<Vec<_>>()
    };
    let registry = super::super::agent_execution::registry(host)?;
    for (handle, source, identity) in actors {
        let mut execution_owner = None;
        handle.refresh_retained_state()?;
        if let Some(loss) = handle.continuity_loss() {
            let execution = !loss.affected_runs.is_empty();
            let owner = capture_execution_owner(
                &registry,
                &handle,
                &identity,
                &source,
                execution,
                &mut execution_owner,
            )?;
            let changed = {
                let mut registry = registry.lock().unwrap_or_else(|p| p.into_inner());
                require_execution_owner(&registry, owner, execution)?;
                registry.commit_native_actor_loss(
                    identity.instance,
                    &identity.record,
                    &source,
                    &loss.affected_runs,
                    &loss.reason,
                    chat::now_ms().unwrap_or(0),
                )?
            };
            if changed {
                super::super::agent_execution::announce(host, &registry);
            }
            let current = host
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .chats
                .get(&source.id)
                .is_some_and(|record| {
                    record.handle.same_attachment(&handle)
                        && record.execution_instance == identity.instance
                        && record.session == source
                });
            if !current {
                return Err(
                    "Retained startup ownership changed before loss acknowledgement".into(),
                );
            }
            handle.ack_continuity_loss(loss.id)?;
        }
    }
    Ok(())
}

/// A completed task from a previous kernel boot is saved history, not ownership
/// of a current cleanup-only helper using the same saved allocation. Archive
/// only an already removed presentation, before orphan adoption consults it.
pub(in crate::host) fn reconcile_previous_boot_history(host: &SharedHost) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(30);
    let (proof, registry) = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        let Some(registry) = state.execution_registry.clone() else {
            // A damaged model still permits exact orphan-terminal recovery.
            // Preserve native history until its registry can be opened; do not
            // prevent the recovery-only listener from exposing checked Close.
            return Ok(());
        };
        (state.proof().clone(), registry)
    };
    let mut after = None;
    loop {
        let page = registry
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .native_actor_ownerships(after, 128)?;
        if page.is_empty() {
            return Ok(());
        }
        for ownership in page {
            after = Some(ownership.actor());
            if Instant::now() >= deadline {
                tracing::warn!(
                    "Historical execution inspection reached its startup budget; remaining records were preserved"
                );
                return Ok(());
            }
            if !ownership.is_pending() || ownership.owner().boot != proof.boot_id {
                continue;
            }
            if !registry
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .has_previous_boot_execution(ownership.source().id, &proof.boot_id)?
            {
                continue;
            }
            let inspection = match Handle::inspect_remote_until(
                ownership.owner(),
                proof.clone(),
                deadline,
            ) {
                Ok(inspection) => inspection,
                Err(error) => {
                    tracing::warn!(actor=%ownership.actor(), %error, "Historical execution cleanup could not authenticate its helper; original records preserved");
                    continue;
                }
            };
            if inspection.identity.instance != ownership.actor()
                || &inspection.identity.record != ownership.owner()
                || (ownership.source() != &inspection.source
                    && !inspection.previous_sources.contains(ownership.source()))
                || ownership.scope()
                    != inspection
                        .launch_account_scope
                        .as_ref()
                        .map_err(String::as_str)
                || !inspection.state.close.completed
                || !inspection.state.close.exited
                || !inspection.state.installed
                || inspection.state.owns_work
                || inspection.state.outbox_ack != inspection.state.outbox_latest
                || inspection.state.continuity_loss.is_some()
            {
                continue;
            }
            // Reattach only for cleanup. This republishes the already closed
            // snapshot under our authenticated epoch, without provider input.
            let handle = match Handle::adopt_remote_for_close(
                ownership.owner(),
                inspection.source.clone(),
                proof.clone(),
                |_| {},
            ) {
                Ok(handle) => handle,
                Err(error) => {
                    tracing::warn!(actor=%ownership.actor(), %error, "Historical helper cleanup remains retryable");
                    continue;
                }
            };
            if handle.actor_identity().as_ref() != Some(&inspection.identity)
                || handle.launch_account_scope() != ownership.scope()
                || !handle.retained_close_proved(&inspection.source, &proof)?
            {
                continue;
            }
            let current = {
                let state = host.lock().unwrap_or_else(|p| p.into_inner());
                !state.shutting_down
                    && state.proof() == &proof
                    && !state.chats.contains_key(&ownership.source().id)
            };
            if !current {
                continue;
            }
            let mut issued = inspection.previous_sources;
            issued.push(inspection.source.clone());
            let archived = registry
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .archive_previous_boot_execution(
                    &ownership,
                    &proof.boot_id,
                    &issued,
                    &inspection.source,
                )?;
            if archived {
                // Retire performs the existing final semantic ACK/real exit
                // checks. Archival supplied no alias or execution authority.
                if let Err(error) = publish_retirement(host, &handle, &inspection.source) {
                    tracing::warn!(actor=%ownership.actor(), %error, "Historical task was archived; authenticated helper journal remains for cleanup retry");
                } else {
                    super::super::agent_execution::retired(host, &inspection.source)?;
                }
            }
        }
    }
}

/// Finish a durable retirement whose helper completed before the prior daemon
/// could commit its registry receipt. No attachment or provider is created.
/// A missing receipt remains unknown; a roster row alone never proves closure.
pub(in crate::host) fn reconcile_retired_actors(host: &SharedHost) -> Result<(), String> {
    let proof = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        if state.shutting_down {
            return Err("Daemon shutdown interrupted retained retirement reconciliation".into());
        }
        state.proof().clone()
    };
    let registry = super::super::agent_execution::registry(host)?;
    let mut after = None;
    loop {
        let page = registry
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .native_actor_ownerships(after, 128)?;
        if page.is_empty() {
            return Ok(());
        }
        for ownership in page {
            after = Some(ownership.actor());
            // Runtime receipts from another boot are unavailable evidence, not
            // authority to declare that historical owner retired.
            if ownership.owner().boot != proof.boot_id {
                continue;
            }
            let retired = registry
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .native_actor_retirement(ownership.actor())?;
            if let Some(retired) = retired {
                // A crash can follow receipt commit but precede capability
                // retirement. The existing process-identity CAS preserves any
                // newer authority; this grants no execution from an archive.
                super::super::agent_execution::retired(host, retired.source())?;
                continue;
            }
            let retirement = Handle::inspect_retained_retirement(ownership.owner(), &proof)?;
            if matches!(retirement, RetirementInspection::ExitedUnproved) {
                // This actor remains unavailable, but its unproved exit does
                // not make unrelated agents or daemon startup unavailable.
                // Do not enter the live cleanup branch or commit retirement.
                tracing::warn!(actor=%ownership.actor(), "Retained helper exited without verified retirement; exact ownership remains retained");
                continue;
            }
            if matches!(retirement, RetirementInspection::Running) {
                // Ordinary orphan recovery had its opportunity first. A durable
                // Pending row left without a HOST record is cleanup-only, even
                // when its Agent was closed or its captured home was replaced.
                // Re-read phase after any normal recovery/promotion and never
                // turn this census into permission to send provider input.
                let pending = registry
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .native_actor_ownership(ownership.actor())?
                    .is_some_and(|current| current.is_pending());
                let attached = host
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .chats
                    .values()
                    .any(|record| {
                        record
                            .handle
                            .actor_identity()
                            .is_some_and(|identity| identity.instance == ownership.actor())
                    });
                if pending && !attached {
                    if let Err(error) =
                        close_unattached(host, ownership.source(), Some(ownership.owner().clone()))
                    {
                        // Actor-local notification/retirement failures keep the
                        // same durable intent and close-only retry owner. They
                        // must not make unrelated native chats or the daemon
                        // unavailable. Unknown census/global lease failures
                        // still stop reconciliation instead of defaulting away.
                        let current = registry
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .native_actor_ownership(ownership.actor())?
                            .ok_or("Pending cleanup lost its durable ownership intent")?;
                        if current != ownership {
                            return Err(
                                "Pending ownership changed during failed cleanup reconciliation"
                                    .into(),
                            );
                        }
                        let paths = crate::daemon::RuntimePaths::discover()
                            .map_err(|error| error.to_string())?;
                        if crate::daemon::lease_owner(&paths.lock, &proof)
                            .map_err(|error| error.to_string())?
                            != std::process::id()
                            || {
                                let state = host.lock().unwrap_or_else(|p| p.into_inner());
                                state.shutting_down || state.proof() != &proof
                            }
                        {
                            return Err(
                                "Daemon ownership changed during pending cleanup reconciliation"
                                    .into(),
                            );
                        }
                        let diagnostic = error.chars().take(1024).collect::<String>();
                        tracing::warn!(actor=%ownership.actor(), error=%diagnostic,
                            "Pending retained actor cleanup remains retryable; durable ownership was preserved");
                    }
                }
                continue;
            }
            // Execution exit and parent wait collection are distinct. Only the
            // authenticated capsule, never the holder's numeric PID lifetime,
            // authorizes consuming this exact final publication.
            let RetirementInspection::Verified(verified) = retirement else {
                unreachable!("running and unproved retirements were handled above")
            };
            let receipt = verified.receipt();
            if receipt.identity.instance != ownership.actor()
                || &receipt.identity.record != ownership.owner()
                || (&receipt.source != ownership.source()
                    && !receipt.previous_sources.contains(ownership.source()))
                || match (ownership.scope(), receipt.launch_account_scope.as_ref()) {
                    (Ok(old), Ok(new)) => old != new,
                    (Err(old), Err(new)) => old != new.as_str(),
                    _ => true,
                }
            {
                return Err(
                    "Retired helper receipt changed its durable actor, source or captured account"
                        .into(),
                );
            }
            {
                let state = host.lock().unwrap_or_else(|p| p.into_inner());
                if state.shutting_down
                    || state.proof() != &proof
                    || state.chats.values().any(|record| {
                        record
                            .handle
                            .actor_identity()
                            .is_some_and(|identity| identity.instance == ownership.actor())
                    })
                {
                    return Err(
                        "Daemon ownership changed while reconciling a retired helper".into(),
                    );
                }
            }
            registry
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .commit_native_actor_retirement(
                    ownership.actor(),
                    ownership.owner(),
                    &receipt.source,
                    receipt
                        .launch_account_scope
                        .as_ref()
                        .map_err(String::as_str),
                    receipt.confirmed_session.as_deref(),
                )?;
        }
    }
}

/// Provider Close and helper retirement are separate. Keep its reducer/Store
/// and final semantic journal until every required daemon write is committed.
pub(in crate::host) fn finish_close(
    host: &SharedHost,
    handle: &Handle,
    requested: &Session,
) -> Result<(), String> {
    let Some(identity) = handle.actor_identity() else {
        return Ok(());
    };
    let (proof, notifications) = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        (state.proof().clone(), state.notifications.clone())
    };
    match Handle::inspect_retained_retirement(&identity.record, &proof)? {
        RetirementInspection::Verified(verified) => {
            return publish_verified_retirement(host, handle, requested, verified);
        }
        RetirementInspection::ExitedUnproved => {
            return Err("The closing helper exited without verified semantic retirement".into());
        }
        RetirementInspection::Running => {}
    }
    let registry = super::super::agent_execution::registry(host)?;
    let mut execution_owner = None;
    let source = {
        let registry = registry.lock().unwrap_or_else(|p| p.into_inner());
        match registry
            .agent_for_session(requested.id)?
            .and_then(|agent| agent.session)
        {
            Some(crate::agent_execution::HostedSession::Chat(source)) => source,
            Some(_) => return Err("The closing native actor changed execution surface".into()),
            None => requested.clone(),
        }
    };
    if !handle.retained_close_proved(&source, &proof)? {
        return Err(
            "The final actor publication does not prove this exact execution descriptor closed"
                .into(),
        );
    }
    handle.refresh_retained_state()?;
    let loss = handle.continuity_loss();
    if let Some(loss) = &loss {
        let execution = !loss.affected_runs.is_empty();
        let owner = capture_execution_owner(
            &registry,
            &handle,
            &identity,
            &source,
            execution,
            &mut execution_owner,
        )?;
        let changed = {
            let mut registry = registry.lock().unwrap_or_else(|p| p.into_inner());
            require_execution_owner(&registry, owner, execution)?;
            registry.commit_native_actor_loss(
                identity.instance,
                &identity.record,
                &source,
                &loss.affected_runs,
                &loss.reason,
                chat::now_ms().unwrap_or(0),
            )?
        };
        if changed {
            super::super::agent_execution::announce(host, &registry);
        }
        let current = {
            let state = host.lock().unwrap_or_else(|p| p.into_inner());
            state.proof() == &proof
                && state
                    .chats
                    .get(&source.id)
                    .is_some_and(|record| record.handle.same_attachment(handle))
        };
        if !current {
            return Err(
                "Actor close ownership changed before continuity-loss acknowledgement".into(),
            );
        }
        handle.ack_continuity_loss(loss.id)?;
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let page = handle.uncommitted_outcomes()?;
        if page.is_empty() {
            break;
        }
        for entry in page {
            let event = match entry.update {
                Update::Execution { run, update } => NativeActorEvent::Execution { run, update },
                Update::Attention(evidence) => NativeActorEvent::Attention(evidence),
                Update::AttentionContext { evidence, content } => {
                    NativeActorEvent::AttentionContext { evidence, content }
                }
                _ => {
                    return Err(
                        "Closing actor journal contains an unsupported semantic event".into(),
                    );
                }
            };
            let execution = matches!(event, NativeActorEvent::Execution { .. });
            let owner = capture_execution_owner(
                &registry,
                &handle,
                &identity,
                &source,
                execution,
                &mut execution_owner,
            )?;
            let (changed, automation) = {
                let mut registry = registry.lock().unwrap_or_else(|p| p.into_inner());
                require_execution_owner(&registry, owner, execution)?;
                let changed = registry.commit_native_actor_event(
                    identity.instance,
                    &identity.record,
                    &source,
                    entry.sequence,
                    &event,
                    chat::now_ms().unwrap_or(0),
                )?;
                let automation = registry.agent_for_session(source.id)?.is_some_and(|agent| {
                    matches!(
                        agent.origin,
                        crate::agent_execution::Origin::Automation { .. }
                    )
                });
                (changed, automation)
            };
            if changed {
                super::super::agent_execution::announce(host, &registry);
            }
            if let Some((evidence, content)) = event.attention()
                && !automation
                && !(loss.is_some() && evidence.kind == crate::notifications::Kind::Succeeded)
            {
                let mut notification = crate::notifications::Event::session(
                    source.id,
                    source.launch.provider,
                    source.launch.recipe.working_directory.clone(),
                    evidence,
                );
                notification.content = content;
                notifications
                    .as_ref()
                    .ok_or("Final attention journal is unavailable; helper is retained")?
                    .record_committed(notification)?
                    .recv_timeout(Duration::from_secs(10))
                    .map_err(|_| "Final attention has not committed; helper is retained")??;
            }
            let current = {
                let state = host.lock().unwrap_or_else(|p| p.into_inner());
                state.proof() == &proof
                    && state
                        .chats
                        .get(&source.id)
                        .is_some_and(|record| record.handle.same_attachment(handle))
            };
            if !current {
                return Err("Actor close ownership changed before final acknowledgement".into());
            }
            handle.commit_retained_outcomes(entry.sequence)?;
            if Instant::now() >= deadline {
                return Err("Final actor outcomes remain pending; retry its retained close".into());
            }
        }
    }
    publish_retirement(host, handle, &source)
}

/// Transfer the exact final proof before native cleanup consumes its temporary
/// file. Unknown account/confirmed identity remains explicit in this actor-only
/// proof and never creates session read/delete authority.
pub(super) fn publish_retirement(
    host: &SharedHost,
    handle: &Handle,
    source: &Session,
) -> Result<(), String> {
    let identity = handle
        .actor_identity()
        .ok_or("Final retirement lost its exact retained actor identity")?;
    let proof = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .proof()
        .clone();
    let verified = match Handle::inspect_retained_retirement(&identity.record, &proof)? {
        RetirementInspection::Verified(verified) => verified,
        RetirementInspection::Running => {
            handle.finish_retained_close()?;
            match Handle::inspect_retained_retirement(&identity.record, &proof)? {
                RetirementInspection::Verified(verified) => verified,
                RetirementInspection::Running | RetirementInspection::ExitedUnproved => {
                    return Err(
                        "Exact semantic retirement remains unproved; ownership remains retained"
                            .into(),
                    );
                }
            }
        }
        RetirementInspection::ExitedUnproved => {
            return Err(
                "Exact semantic retirement receipt is unavailable; ownership remains retained"
                    .into(),
            );
        }
    };
    publish_verified_retirement(host, handle, source, verified)
}

fn publish_verified_retirement(
    host: &SharedHost,
    handle: &Handle,
    source: &Session,
    verified: VerifiedRetirement,
) -> Result<(), String> {
    let identity = handle
        .actor_identity()
        .ok_or("Final retirement lost its exact retained actor identity")?;
    let receipt = verified.receipt();
    if receipt.identity != identity
        || (&receipt.source != source && !receipt.previous_sources.contains(source))
        || receipt
            .launch_account_scope
            .as_ref()
            .map_err(String::as_str)
            != handle.launch_account_scope()
    {
        return Err("Final retirement changed its exact actor, source or captured account".into());
    }
    verify_close_ownership(
        host,
        &identity,
        &receipt.source,
        &receipt.previous_sources,
        receipt
            .launch_account_scope
            .as_ref()
            .map_err(String::as_str),
    )?;
    let registry = super::super::agent_execution::registry(host)?;
    registry
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .commit_native_actor_retirement(
            identity.instance,
            &identity.record,
            &receipt.source,
            receipt
                .launch_account_scope
                .as_ref()
                .map_err(String::as_str),
            receipt.confirmed_session.as_deref(),
        )
}

/// No global state lock is held across the durable census publication. A
/// failed publication leaves the exact helper owned, without exposing input.
pub(super) fn register_ownership(
    host: &SharedHost,
    handle: &Handle,
    source: &Session,
) -> Result<(), String> {
    let identity = handle
        .actor_identity()
        .ok_or("Retained actor registration has no authenticated helper identity")?;
    let registry = super::super::agent_execution::registry(host)?;
    registry
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .register_native_actor_ownership(
            identity.instance,
            &identity.record,
            source,
            handle.launch_account_scope(),
        )
}
