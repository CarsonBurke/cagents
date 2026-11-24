//! One coalescing persistence broker for all helper-owned actors. Provider
//! callbacks only wake it; neither callback return nor enqueueing acknowledges
//! a durable outcome. No RPC, SQLite transaction or journal wait holds HostState.
use super::*;
use crate::chat::runtime::{OutboxEntry, RetirementInspection, VerifiedRetirement};
use crate::host::model::NativeActorEvent;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Condvar, Mutex},
};

#[derive(Clone)]
pub(in crate::host) struct Startup {
    pub(super) source: Session,
    pub(super) requested: Session,
    pub(super) context: Arc<chat::protocol::ExecutionContext>,
    /// An initial named resume failed after installation. Only this exact
    /// attachment may finish its normal semantic close before a retry starts.
    pub(super) failed_close: Option<super::CloseTarget>,
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
    #[cfg(test)]
    worker_lifetime: Weak<()>,
}

impl Inbox {
    fn push(&self, id: TerminalId) {
        self.pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(id);
        self.wake.notify_one();
    }
}

#[cfg(test)]
pub(in crate::host) fn broker_running(host: &SharedHost) -> bool {
    host.lock()
        .unwrap()
        .native_actor_outcomes
        .as_ref()
        .is_some_and(|broker| broker.0.worker_lifetime.strong_count() > 0)
}

pub(in crate::host) fn wake(host: &SharedHost, id: TerminalId) {
    let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
    if state.shutting_down {
        return;
    }
    let broker = match state.native_actor_outcomes.clone() {
        Some(broker) => broker,
        None => {
            #[cfg(test)]
            let worker_lifetime = Arc::new(());
            let inbox = Arc::new(Inbox {
                pending: Mutex::new(BTreeSet::new()),
                wake: Condvar::new(),
                #[cfg(test)]
                worker_lifetime: Arc::downgrade(&worker_lifetime),
            });
            let mut outcomes = Outcomes::new(Arc::downgrade(host), inbox.clone(), drain);
            // The broker stops only once the daemon is gone or stopping, when
            // `wake` no longer hands it work; a panicking step is contained,
            // so a registered broker always has a live worker.
            if let Err(error) = crate::host::service::spawn(
                "cagents-actor-outcomes",
                Duration::from_secs(1),
                move || {
                    #[cfg(test)]
                    let _lifetime = &worker_lifetime;
                    outcomes.step()
                },
            ) {
                tracing::error!(%error, "cannot start retained actor outcome broker; helper outbox remains unacknowledged");
                return;
            }
            let broker = Broker(inbox);
            state.native_actor_outcomes = Some(broker.clone());
            broker
        }
    };
    drop(state);
    broker.0.push(id);
}

/// How soon an actor whose drain panicked is drained again, doubling while it
/// keeps panicking. The helper retains every unacknowledged outcome, so a retry
/// recommits from the helper's outbox rather than from anything the panic left.
const PANIC_RETRY: Duration = Duration::from_secs(1);

/// The broker worker's state between steps.
struct Outcomes<D> {
    host: Weak<HostLock>,
    inbox: Arc<Inbox>,
    drain: D,
    /// Actors with another page, or whose last drain failed: revisited each
    /// second.
    retry: BTreeSet<TerminalId>,
    warned: BTreeSet<TerminalId>,
    /// Actors whose last drain panicked, held back until their backoff ends so
    /// a deterministic panic neither spins nor floods diagnostics. They stay
    /// here, rather than being dropped, until a drain returns.
    panicked: BTreeMap<TerminalId, (Instant, crate::host::service::Backoff)>,
    panic_retry: Duration,
}

impl<D: FnMut(&SharedHost, TerminalId) -> Result<bool, String>> Outcomes<D> {
    fn new(host: Weak<HostLock>, inbox: Arc<Inbox>, drain: D) -> Self {
        Self {
            host,
            inbox,
            drain,
            retry: BTreeSet::new(),
            warned: BTreeSet::new(),
            panicked: BTreeMap::new(),
            panic_retry: PANIC_RETRY,
        }
    }

    /// Waits for work, then drains each woken or retried actor once.
    fn step(&mut self) -> crate::host::service::Next {
        use crate::host::service::Next;
        let work = self.next_work();
        let Some(host) = self.host.upgrade() else {
            return Next::Stop;
        };
        if host.lock().unwrap_or_else(|p| p.into_inner()).shutting_down {
            return Next::Stop;
        }
        for id in work {
            self.drain_one(&host, id);
        }
        Next::Continue
    }

    fn next_work(&mut self) -> BTreeSet<TerminalId> {
        let mut pending = self.inbox.pending.lock().unwrap_or_else(|p| p.into_inner());
        if pending.is_empty() {
            let idle = if self.retry.is_empty() {
                Duration::from_secs(5)
            } else {
                Duration::from_secs(1)
            };
            let now = Instant::now();
            let timeout = self
                .panicked
                .values()
                .map(|(due, _)| due.saturating_duration_since(now))
                .fold(idle, Duration::min);
            pending = self
                .inbox
                .wake
                .wait_timeout(pending, timeout)
                .unwrap_or_else(|p| p.into_inner())
                .0;
        }
        let mut work = std::mem::take(&mut *pending);
        drop(pending);
        work.append(&mut self.retry);
        let now = Instant::now();
        for (&id, (due, _)) in &self.panicked {
            if *due <= now {
                work.insert(id);
            } else {
                work.remove(&id);
            }
        }
        work
    }

    fn drain_one(&mut self, host: &SharedHost, id: TerminalId) {
        let drained = crate::host::service::contain("retained actor outcome drain", || {
            (self.drain)(host, id)
        });
        match drained {
            Ok(Ok(more)) => {
                self.warned.remove(&id);
                self.panicked.remove(&id);
                if more {
                    self.retry.insert(id);
                }
            }
            Ok(Err(error)) => {
                self.panicked.remove(&id);
                if self.warned.insert(id) {
                    tracing::warn!(%id, %error, "retained actor outcome is not committed; helper retains it for retry");
                }
                self.retry.insert(id);
            }
            Err(panicked) => {
                let (due, backoff) = self.panicked.entry(id).or_insert_with(|| {
                    (
                        Instant::now(),
                        crate::host::service::Backoff::new(self.panic_retry),
                    )
                });
                let wait = backoff.fail();
                *due = Instant::now() + wait;
                tracing::error!(%id, %panicked, retry_seconds = wait.as_secs_f64(),
                    "retained actor outcome drain panicked; helper retains the outcome for retry");
            }
        }
    }
}

#[cfg(test)]
mod outcome_broker_tests {
    use super::*;
    use std::sync::mpsc;

    const PROMPT: Duration = Duration::from_secs(10);

    fn inbox() -> Arc<Inbox> {
        Arc::new(Inbox {
            pending: Mutex::new(BTreeSet::new()),
            wake: Condvar::new(),
            worker_lifetime: Weak::new(),
        })
    }

    /// A drain that panics while the same step still holds other actors must
    /// not lose them, and the broker must keep acknowledging later outcomes.
    #[test]
    fn a_panicking_drain_does_not_end_acknowledgements() {
        let host = crate::host::service::test_host();
        let inbox = inbox();
        let (acknowledged, acknowledgements) = mpsc::channel();
        // Ordered ids: a step drains `failing` first, then `batched`.
        let [failing, batched, later] =
            [1, 2, 3].map(|id| TerminalId::from_uuid(uuid::Uuid::from_u128(id)));
        let attempts = Arc::new(Mutex::new(Vec::new()));
        let mut outcomes = Outcomes::new(Arc::downgrade(&host), inbox.clone(), {
            let attempts = attempts.clone();
            let mut failures = 0;
            move |_: &SharedHost, id: TerminalId| {
                attempts.lock().unwrap().push(id);
                if id == failing && failures < 2 {
                    failures += 1;
                    panic!("drain fixture panics for {id}");
                }
                acknowledged.send(id).unwrap();
                Ok(false)
            }
        });
        outcomes.panic_retry = Duration::from_millis(20);
        let broker = crate::host::service::spawn(
            "outcome-broker-test",
            Duration::from_millis(1),
            move || outcomes.step(),
        )
        .unwrap();

        // One step takes both, so the panic happens while it still holds the
        // other actor.
        inbox.pending.lock().unwrap().extend([failing, batched]);
        inbox.wake.notify_one();
        assert_eq!(acknowledgements.recv_timeout(PROMPT), Ok(batched));
        assert_eq!(
            attempts.lock().unwrap()[..2],
            [failing, batched],
            "the batch-mate is drained after the panic in the same step"
        );
        inbox.push(later);
        // The panicked actor is retried after its backoff, without another
        // wake, until it drains.
        let rest: BTreeSet<_> = (0..2)
            .map(|_| acknowledgements.recv_timeout(PROMPT).unwrap())
            .collect();
        assert_eq!(rest, BTreeSet::from([failing, later]));
        assert!(!broker.is_finished());

        host.lock().unwrap().shutting_down = true;
        inbox.wake.notify_one();
        broker.join().expect("the broker stops only for shutdown");
    }

    #[test]
    fn a_panicked_actor_waits_out_its_backoff_even_when_woken() {
        let host = crate::host::service::test_host();
        let inbox = inbox();
        let drains = Arc::new(Mutex::new(Vec::new()));
        let mut outcomes = Outcomes::new(Arc::downgrade(&host), inbox.clone(), {
            let drains = drains.clone();
            move |_: &SharedHost, id: TerminalId| -> Result<bool, String> {
                drains.lock().unwrap().push(id);
                panic!("drain fixture always panics")
            }
        });
        outcomes.panic_retry = Duration::from_secs(60 * 60);
        let id = TerminalId::new();
        inbox.push(id);
        assert_eq!(outcomes.step(), crate::host::service::Next::Continue);
        assert!(outcomes.panicked.contains_key(&id));
        inbox.push(id);
        assert_eq!(outcomes.step(), crate::host::service::Next::Continue);
        assert_eq!(*drains.lock().unwrap(), [id], "woken during its backoff");
        assert!(
            outcomes.panicked.contains_key(&id),
            "the outcome is still owed"
        );
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

/// The descriptor `persist_confirmed_identity` publishes for this attachment:
/// its resident source with the conversation the helper itself confirmed.
/// Read it before taking the Registry lock.
fn confirmed_projection(handle: &Handle, source: &Session) -> Option<Session> {
    Some(projection(source, handle.confirmed_session_id()?))
}

/// `source` naming the provider conversation `confirmed`.
fn projection(source: &Session, confirmed: String) -> Session {
    let mut projected = source.clone();
    projected.launch.session_id = Some(confirmed);
    projected
}

/// Recheck the exact captured row under the same Registry mutex as settlement.
/// Revocation, mutation or retirement after helper recognition cannot authorize
/// a durable run transition or the helper ACK that follows it. The single
/// accepted change is a concurrent identity confirmation: it keeps every
/// authority field and publishes exactly `confirmed`, the attachment's
/// resident source with the conversation its helper confirmed.
fn require_execution_owner(
    registry: &super::super::model::Registry,
    captured: Option<&ExecutionOwner>,
    execution: bool,
    confirmed: Option<&Session>,
) -> Result<(), String> {
    if !execution {
        return Ok(());
    }
    let captured = captured.ok_or("Retained execution lacks authenticated source ownership")?;
    let owner = registry
        .native_actor_ownership(captured.0.actor())?
        .ok_or("Retained execution lost its durable ownership registration")?;
    let current = owner == captured.0
        || confirmed.is_some_and(|confirmed| owner == captured.0.with_source(confirmed));
    if !current || owner.is_pending() {
        return Err("Retained execution ownership changed after source authentication; evidence remains unacknowledged".into());
    }
    if registry.native_actor_retirement(owner.actor())?.is_some() {
        return Err("A retired actor cannot commit execution or affected-run loss; evidence remains unacknowledged".into());
    }
    Ok(())
}

/// A run transition requires the conversation it followed to be durable in the
/// actor census first: `persist_identity` publishes it, and this refuses a
/// transition that would outrun it. The helper journals with each outcome the
/// conversation it had confirmed then, so a later reset that clears or replaces
/// the helper's conversation cannot release an outcome whose own conversation
/// was never published. Outcomes from a helper that predates that record, and
/// continuity losses, require the helper's current conversation: the helper
/// confirms before it journals the outcomes that follow, and every outcome read
/// refreshes that identity after reading its page. Close-only cleanup
/// attachments publish no identity and require none.
fn require_published_identity(
    registry: &super::super::model::Registry,
    actor: uuid::Uuid,
    required: Option<&str>,
) -> Result<(), String> {
    let Some(required) = required else {
        return Ok(());
    };
    let owner = registry
        .native_actor_ownership(actor)?
        .ok_or("Retained execution lost its durable ownership registration")?;
    if owner.source().launch.session_id.as_deref() != Some(required) {
        return Err("The provider's confirmed conversation is not yet durable; its run outcome remains unacknowledged".into());
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

    fn confirmed_projection(&self) -> Option<Session> {
        confirmed_projection(&self.handle, &self.source)
    }

    /// The conversation `require_published_identity` requires before an
    /// `execution` transition: the one `followed` names when the helper
    /// recorded it with the outcome, otherwise the helper's current one. A
    /// close-only cleanup attachment publishes none. Read it before taking the
    /// Registry lock.
    fn required_identity(&self, execution: bool, followed: Option<&str>) -> Option<String> {
        if !execution || self.handle.is_close_only() {
            return None;
        }
        followed
            .map(str::to_owned)
            .or_else(|| self.handle.confirmed_session_id())
    }

    /// The census descriptor an outcome requiring `required` is checked
    /// against: its projection, or the current one when it requires none.
    fn required_projection(&self, required: Option<&String>) -> Option<Session> {
        match required {
            Some(required) => Some(projection(&self.source, required.clone())),
            None => self.confirmed_projection(),
        }
    }

    /// Publish the conversation an execution outcome requires. Publication
    /// replaces the census row an earlier capture authenticated, so it drops
    /// that capture; the next commit authenticates the published row.
    fn publish_required(
        &self,
        host: &SharedHost,
        registry: &super::super::agent_execution::SharedRegistry,
        required: Option<&str>,
        execution_owner: &mut Option<ExecutionOwner>,
    ) -> Result<(), String> {
        if persist_identity(host, self, registry, required)? {
            *execution_owner = None;
        }
        Ok(())
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
        let confirmed = self.target.confirmed_projection();
        require_execution_owner(
            &self.registry.lock().unwrap_or_else(|p| p.into_inner()),
            self.owner.as_ref(),
            true,
            confirmed.as_ref(),
        )
    }

    fn require_committed_through(&self, through: u64) -> Result<(), String> {
        let confirmed = self.target.confirmed_projection();
        let registry = self.registry.lock().unwrap_or_else(|p| p.into_inner());
        require_execution_owner(&registry, self.owner.as_ref(), true, confirmed.as_ref())?;
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
                    entry,
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

/// Persist only the helper's exact confirmed projection. Keep the resident
/// launch alias unchanged so concurrent controls retain their original process
/// fences. Failed writes stay on the broker's retry set, without a UI client.
fn persist_confirmed_identity(
    host: &SharedHost,
    target: &SemanticTarget,
    registry: &super::super::agent_execution::SharedRegistry,
) -> Result<bool, String> {
    persist_identity(
        host,
        target,
        registry,
        target.handle.confirmed_session_id().as_deref(),
    )
}

/// Publish the projection of `confirmed`, a conversation the helper confirmed,
/// to the agent and the actor census. True when it replaced the census row.
fn persist_identity(
    host: &SharedHost,
    target: &SemanticTarget,
    registry: &super::super::agent_execution::SharedRegistry,
    confirmed: Option<&str>,
) -> Result<bool, String> {
    if target.handle.is_close_only() {
        return Ok(false);
    }
    let Some(projected) = confirmed.map(|confirmed| projection(&target.source, confirmed.into()))
    else {
        return Ok(false);
    };
    let (ownership, bound) = {
        let registry = registry.lock().unwrap_or_else(|p| p.into_inner());
        (
            registry.native_actor_ownership(target.identity.instance)?,
            registry
                .agent_for_session(target.source.id)?
                .and_then(|agent| agent.session),
        )
    };
    // Cleanup intent can report attention without installed writer authority.
    // Keep its existing semantic drain; it cannot publish a new identity.
    let Some(ownership) = ownership.filter(|owner| !owner.is_pending()) else {
        return Ok(false);
    };
    let confirmed = &projected.launch.session_id;
    if &ownership.source().launch.session_id == confirmed
        && bound.as_ref().is_none_or(|session| {
            matches!(session,
            crate::agent_execution::HostedSession::Chat(source)
                if &source.launch.session_id == confirmed)
        })
    {
        return Ok(false);
    }
    let owner = ExecutionOwner::authenticate(
        Some(ownership),
        &target.identity,
        &target.source,
        target.handle.launch_account_scope(),
        |sources| target.handle.recognize_retained_sources(sources),
    )?;
    target
        .handle
        .recognize_retained_sources(std::slice::from_ref(&projected))?;
    target.require_current(host)?;
    let mut aliases = vec![owner.0.source().clone(), target.source.clone()];
    // A startup reply may have bound an earlier issued conversation after the
    // census advanced. Authenticate that durable alias too before replacing it.
    if let Some(crate::agent_execution::HostedSession::Chat(bound)) = bound
        && !aliases.contains(&bound)
    {
        target
            .handle
            .recognize_retained_sources(std::slice::from_ref(&bound))?;
        aliases.push(bound);
    }
    let aliases = aliases
        .into_iter()
        .map(crate::agent_execution::HostedSession::Chat)
        .collect::<Vec<_>>();
    let (changed, registered) = {
        let mut registry = registry.lock().unwrap_or_else(|p| p.into_inner());
        // Publication replaces exactly the authenticated row. A concurrent
        // drain or close may already have published this same projection.
        require_execution_owner(&registry, Some(&owner), true, Some(&projected))?;
        let changed = registry.adopt_authenticated_agent_session(
            &aliases,
            &crate::agent_execution::HostedSession::Chat(projected.clone()),
        )?;
        let published = owner.0.with_source(&projected);
        let registered = if registry
            .native_actor_ownership(owner.0.actor())?
            .is_some_and(|current| current == published)
        {
            Ok(false)
        } else {
            registry
                .confirm_native_actor_identity(&owner.0, &projected)
                .map(|()| true)
        };
        (changed, registered)
    };
    // Adoption may have committed even if census publication needs retry.
    if changed {
        super::super::agent_execution::announce(host, registry);
    }
    registered
}

fn drain(host: &SharedHost, id: TerminalId) -> Result<bool, String> {
    let target = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        let Some(record) = state.chats.get(&id) else {
            return Ok(false);
        };
        let Some(identity) = record.handle.actor_identity() else {
            return Ok(false);
        };
        SemanticTarget {
            handle: record.handle.clone(),
            source: record.session.clone(),
            identity,
            notifications: state.notifications.clone(),
        }
    };
    let registry = super::super::agent_execution::registry(host)?;
    let mut execution_owner = None;
    target.handle.refresh_retained_state()?;
    persist_confirmed_identity(host, &target, &registry)?;
    #[cfg(test)]
    drain_fence::pass(id);
    let SemanticTarget {
        handle,
        source,
        identity,
        ..
    } = &target;
    let loss = handle.continuity_loss();
    if let Some(loss) = &loss {
        let execution = !loss.affected_runs.is_empty();
        let owner = capture_execution_owner(
            &registry,
            handle,
            identity,
            source,
            execution,
            &mut execution_owner,
        )?;
        let confirmed = target.confirmed_projection();
        let required = target.required_identity(execution, None);
        let changed = {
            let mut registry = registry.lock().unwrap_or_else(|p| p.into_inner());
            require_execution_owner(&registry, owner, execution, confirmed.as_ref())?;
            require_published_identity(&registry, identity.instance, required.as_deref())?;
            registry.commit_native_actor_loss(
                identity.instance,
                &identity.record,
                source,
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
                record.handle.same_attachment(handle)
                    && record.execution_instance == identity.instance
                    && record.session == *source
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
    let deadline = Instant::now() + Duration::from_secs(30);
    for entry in page {
        commit_semantic_entry(
            host,
            &target,
            &registry,
            &mut execution_owner,
            entry,
            loss.is_some(),
            deadline,
        )?;
    }
    if more {
        // An outcome that followed a conversation since reset republished it.
        persist_confirmed_identity(host, &target, &registry)?;
    }
    // One bounded page per actor per pass. A detached actor can have many
    // pages without another latest-sequence edge; explicitly revisit it until
    // an empty page confirms the helper's entire outbox is acknowledged.
    Ok(more)
}

/// Test-only pause in a broker drain after its identity publication and before
/// its outcome read, so a test can confirm a conversation inside that window.
#[cfg(test)]
pub(in crate::host) mod drain_fence {
    use super::TerminalId;
    use std::{
        collections::BTreeMap,
        sync::{Mutex, mpsc},
        time::Duration,
    };

    type Armed = (mpsc::SyncSender<()>, mpsc::Receiver<()>);
    static ARMED: Mutex<BTreeMap<TerminalId, Armed>> = Mutex::new(BTreeMap::new());

    pub(in crate::host) struct Fence {
        reached: mpsc::Receiver<()>,
        release: mpsc::SyncSender<()>,
    }

    impl Fence {
        pub(in crate::host) fn wait_reached(&self, within: Duration) {
            self.reached
                .recv_timeout(within)
                .expect("the outcome drain never reached its fence");
        }

        pub(in crate::host) fn release(self) {
            let _ = self.release.send(());
        }
    }

    /// Holds the next drain of `id` once, until the fence is released.
    pub(in crate::host) fn arm(id: TerminalId) -> Fence {
        let (reached_tx, reached) = mpsc::sync_channel(1);
        let (release, release_rx) = mpsc::sync_channel(1);
        let previous = ARMED
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(id, (reached_tx, release_rx));
        assert!(previous.is_none(), "one drain fence per actor");
        Fence { reached, release }
    }

    pub(super) fn pass(id: TerminalId) {
        let Some((reached, release)) = ARMED.lock().unwrap_or_else(|p| p.into_inner()).remove(&id)
        else {
            return;
        };
        let _ = reached.send(());
        // A dropped fence releases the drain; a forgotten one cannot hang it.
        let _ = release.recv_timeout(Duration::from_secs(60));
    }
}

fn commit_attention_clear(
    notifications: Option<&super::super::notifications::Handle>,
    session: TerminalId,
    actor: uuid::Uuid,
    sequence: u64,
    clear: crate::agent::observe::AttentionClear,
    timeout: Duration,
) -> Result<(), String> {
    use crate::agent::observe::AttentionClear;
    let notifications = notifications
        .ok_or("The notification journal is unavailable; attention remains retained")?;
    let receipt = match clear {
        AttentionClear::TurnStarted => {
            notifications.acknowledge_turn_committed(session, actor, sequence)
        }
        AttentionClear::LeftWaiting => {
            notifications.acknowledge_waiting_committed(session, actor, sequence)
        }
    }?;
    receipt
        .recv_timeout(timeout)
        .map_err(|_| "The notification journal has not confirmed this attention change")?
}

/// The broker and synchronous Answer boundary share the same semantic writes,
/// exact owner checks, notification receipts and commit-before-ACK ordering.
fn commit_semantic_entry(
    host: &SharedHost,
    target: &SemanticTarget,
    registry: &super::super::agent_execution::SharedRegistry,
    execution_owner: &mut Option<ExecutionOwner>,
    entry: OutboxEntry,
    lost: bool,
    deadline: Instant,
) -> Result<(), String> {
    if Instant::now() >= deadline {
        return Err("Retained semantic acknowledgement deadline expired".into());
    }
    target.require_current(host)?;
    let OutboxEntry {
        sequence,
        update,
        confirmed_session: followed,
    } = entry;
    let semantic = match update {
        Update::Execution { run, update } => NativeActorEvent::Execution { run, update },
        Update::Attention(evidence) => NativeActorEvent::Attention(evidence),
        Update::AttentionContext { evidence, content } => {
            NativeActorEvent::AttentionContext { evidence, content }
        }
        Update::AttentionCleared(clear) => NativeActorEvent::AttentionCleared(clear),
        _ => return Err("The retained semantic outbox contains an unsupported event".into()),
    };
    let execution = matches!(semantic, NativeActorEvent::Execution { .. });
    let required = target.required_identity(execution, followed.as_deref());
    // The page was read before the identity it follows was refreshed.
    target.publish_required(host, registry, required.as_deref(), execution_owner)?;
    capture_execution_owner(
        registry,
        &target.handle,
        &target.identity,
        &target.source,
        execution,
        execution_owner,
    )?;
    let authenticated = execution || execution_owner.is_some();
    let confirmed = target.required_projection(required.as_ref());
    let (changed, automation) = {
        let mut registry = registry.lock().unwrap_or_else(|p| p.into_inner());
        require_execution_owner(
            &registry,
            execution_owner.as_ref(),
            authenticated,
            confirmed.as_ref(),
        )?;
        require_published_identity(&registry, target.identity.instance, required.as_deref())?;
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
    if let NativeActorEvent::AttentionCleared(clear) = semantic
        && !automation
    {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or("Retained semantic acknowledgement deadline expired")?;
        commit_attention_clear(
            target.notifications.as_ref(),
            target.source.id,
            target.identity.instance,
            sequence,
            clear,
            remaining.min(Duration::from_secs(10)),
        )?;
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
    let confirmed = target.confirmed_projection();
    require_execution_owner(
        &registry.lock().unwrap_or_else(|p| p.into_inner()),
        execution_owner.as_ref(),
        authenticated,
        confirmed.as_ref(),
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
    if state
        .native_actor_startups
        .get(&source.id)
        .is_some_and(|pending| &pending.source == source && Arc::ptr_eq(&pending.context, context))
    {
        state.native_actor_startups.remove(&source.id);
        state.inventory_generation += 1;
        crate::daemon::wake();
    }
}

#[cfg(test)]
pub(super) fn reconcile_startup(host: &SharedHost, source: &Session) -> Result<bool, String> {
    reconcile_startup_for_start(host, source).map_err(|failure| failure.to_string())
}

pub(super) fn reconcile_startup_for_start(
    host: &SharedHost,
    source: &Session,
) -> Result<bool, chat::protocol::SetupError> {
    use chat::protocol::SetupError;
    if prepare_failed_start_for_start(host, source)?.is_some() {
        return Ok(true);
    }
    let pending = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        let Some(pending) = state.native_actor_startups.get(&source.id) else {
            return Ok(false);
        };
        if &pending.source != source && &pending.requested != source {
            return Err(SetupError::Invalid(
                "The retained startup belongs to another exact descriptor".into(),
            ));
        }
        if state.chats.contains_key(&source.id) || state.chat_spawning.contains_key(&source.id) {
            return Err(SetupError::Unavailable(
                "The exact retained startup is still transitioning".into(),
            ));
        }
        (pending.source.clone(), pending.context.clone())
    };
    match pending
        .1
        .reconcile_cleanup()
        .map_err(SetupError::Unavailable)?
    {
        chat::protocol::LaunchOutcome::NoProvider
        | chat::protocol::LaunchOutcome::TeardownProved => {
            forget_startup(host, &pending.0, &pending.1);
            Ok(true)
        }
        chat::protocol::LaunchOutcome::Uncertain => Err(SetupError::Unavailable(
            "The exact retained startup has not proved cleanup; no second provider was started"
                .into(),
        )),
    }
}

/// Explicit retry may discover the still-owned failed attachment in ChatList.
/// Preparation finishes that exact normal Close and returns only its original
/// requested launch. An unrelated or successful retained actor never gains a
/// restart grant through this path.
pub(super) fn prepare_failed_start_for_start(
    host: &SharedHost,
    source: &Session,
) -> Result<Option<Session>, chat::protocol::SetupError> {
    use chat::protocol::SetupError;
    let failed = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        match state.native_actor_startups.get(&source.id) {
            Some(pending) => match &pending.failed_close {
                Some(target) if source == &pending.requested || source == &target.source => {
                    Some((pending.clone(), target.clone()))
                }
                Some(_) => {
                    return Err(SetupError::Invalid(
                        "The retained startup belongs to another exact descriptor".into(),
                    ));
                }
                None => None,
            },
            None => None,
        }
    };
    if let Some((pending, target)) = failed {
        if let Err(failure) = super::close_captured(host, target.clone()) {
            // Only the same captured native/terminal owners may finish this
            // cleanup on retry. An identity change needs fresh authority.
            let state = host.lock().unwrap_or_else(|p| p.into_inner());
            let same_source = state.chats.get(&target.source.id).is_some_and(|record| {
                target.matches(record)
                    && record.session.terminal.as_ref().is_none_or(|source| {
                        state.terminals.get(&source.id).is_none_or(|terminal| {
                            super::matches_paired_terminal(&terminal.session, source, state.proof())
                                && record
                                    .paired_terminal
                                    .as_ref()
                                    .is_some_and(|expected| terminal.handle.same_worker(expected))
                        })
                    })
            });
            return Err(if same_source {
                SetupError::Unavailable(failure.message)
            } else {
                SetupError::Invalid(failure.message)
            });
        }
        forget_startup(host, &pending.source, &pending.context);
        return Ok(Some(pending.requested));
    }
    Ok(None)
}

#[cfg(test)]
mod startup_retryability_tests {
    use super::*;
    use chat::protocol::{LaunchCleanup, LaunchOutcome, SetupError};
    use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

    struct Cleanup {
        mode: AtomicU8,
        calls: AtomicUsize,
    }

    impl LaunchCleanup for Cleanup {
        fn cleanup(&self) -> Result<LaunchOutcome, String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match self.mode.load(Ordering::SeqCst) {
                0 => Err("fixture teardown has not committed".into()),
                1 => Ok(LaunchOutcome::Uncertain),
                _ => Ok(LaunchOutcome::TeardownProved),
            }
        }
    }

    fn pending(
        root: &std::path::Path,
    ) -> (SharedHost, Session, Arc<chat::protocol::ExecutionContext>) {
        let proof = crate::model::HostSessionProof {
            host_session_id: "startup-retryability".into(),
            boot_id: crate::daemon::read_boot_id().unwrap(),
            host_incarnation: "current".into(),
        };
        let source = Session {
            id: TerminalId::new(),
            proof: Some(proof.clone()),
            terminal: None,
            owned_codex_listener: None,
            native_continuity: Default::default(),
            launch: chat::Launch {
                provider: Provider::Claude,
                session_id: None,
                recipe: crate::model::LaunchRecipe {
                    program: "/must-not-launch/claude".into(),
                    arguments: vec![],
                    working_directory: root.into(),
                    environment: Default::default(),
                    unset_environment: Default::default(),
                },
                draft_model: None,
            },
        };
        let host = Arc::new(HostLock::new(HostState::new(proof)));
        let context = Arc::new(chat::protocol::ExecutionContext::capture(root).unwrap());
        host.lock().unwrap().native_actor_startups.insert(
            source.id,
            Startup {
                source: source.clone(),
                requested: source.clone(),
                context: context.clone(),
                failed_close: None,
            },
        );
        (host, source, context)
    }

    #[test]
    fn startup_descriptor_conflict_is_permanent_but_exact_transition_can_finish() {
        let root = tempfile::tempdir().unwrap();
        let (host, source, context) = pending(root.path());
        let mut other = source.clone();
        other
            .launch
            .recipe
            .arguments
            .push("--changed-recipe".into());
        host.lock()
            .unwrap()
            .chat_spawning
            .insert(source.id, source.clone());
        assert!(matches!(
            reconcile_startup_for_start(&host, &other),
            Err(SetupError::Invalid(_)),
        ));
        assert!(matches!(
            reconcile_startup_for_start(&host, &source),
            Err(SetupError::Unavailable(_)),
        ));
        assert!(Arc::ptr_eq(
            &host.lock().unwrap().native_actor_startups[&source.id].context,
            &context,
        ));
        host.lock().unwrap().chat_spawning.remove(&source.id);
        assert!(reconcile_startup_for_start(&host, &source).unwrap());
        assert!(host.lock().unwrap().native_actor_startups.is_empty());
        assert!(host.lock().unwrap().chats.is_empty());
    }

    #[test]
    fn startup_cleanup_failure_and_uncertainty_can_be_retried_on_the_same_owner() {
        let root = tempfile::tempdir().unwrap();
        let (host, source, context) = pending(root.path());
        let cleanup = Arc::new(Cleanup {
            mode: AtomicU8::new(0),
            calls: AtomicUsize::new(0),
        });
        context.claim_owner(cleanup.clone());
        for mode in [0, 1] {
            cleanup.mode.store(mode, Ordering::SeqCst);
            assert!(matches!(
                reconcile_startup_for_start(&host, &source),
                Err(SetupError::Unavailable(_)),
            ));
            assert!(Arc::ptr_eq(
                &host.lock().unwrap().native_actor_startups[&source.id].context,
                &context,
            ));
            assert!(host.lock().unwrap().chats.is_empty());
        }
        cleanup.mode.store(2, Ordering::SeqCst);
        assert!(reconcile_startup_for_start(&host, &source).unwrap());
        assert_eq!(cleanup.calls.load(Ordering::SeqCst), 3);
        assert!(host.lock().unwrap().native_actor_startups.is_empty());
        assert_eq!(context.outcome(), LaunchOutcome::TeardownProved);
    }

    /// Tests only entry admission, before any private RPC or provider effect.
    #[test]
    fn retained_restore_distinguishes_fixed_identity_conflicts_from_daemon_shutdown() {
        use crate::daemon::holder_registry::{Kind, Process, Record};
        let root = tempfile::tempdir().unwrap();
        let (host, source, _) = pending(root.path());
        let proof = host.lock().unwrap().proof().clone();
        let process = Process::read(std::process::id() as i32).unwrap();
        let owner = Record {
            version: 1,
            build: Some(crate::daemon::holder_registry::BuildIdentity {
                version: "fixture".into(),
                commit: None,
                capabilities: vec![
                    crate::daemon::holder_registry::RETAINED_CLAUDE_CAPABILITY.into(),
                ],
            }),
            kind: Kind::Native,
            application: crate::instance::APPLICATION_ID.into(),
            boot: proof.boot_id.clone(),
            terminal: source.id,
            holder: process.clone(),
            root: process,
        };
        for conflict in 0..3 {
            let mut requested = source.clone();
            let mut conflicting_owner = owner.clone();
            match conflict {
                0 => conflicting_owner.terminal = TerminalId::new(),
                1 => conflicting_owner.kind = Kind::Terminal,
                _ => requested.launch.provider = Provider::Codex,
            }
            assert!(matches!(
                restore_for_start(&host, 0, &requested, &proof, conflicting_owner),
                Err(SetupError::Invalid(_)),
            ));
        }
        host.lock().unwrap().shutting_down = true;
        assert!(matches!(
            restore_for_start(&host, 0, &source, &proof, owner),
            Err(SetupError::Unavailable(_)),
        ));
        assert!(host.lock().unwrap().chats.is_empty());
    }

    #[test]
    fn pending_enrollment_preserves_exact_owner_source_and_captured_scope() {
        use crate::daemon::holder_registry::{Kind, Process, Record};
        let root = tempfile::tempdir().unwrap();
        let (host, source, _) = pending(root.path());
        let proof = host.lock().unwrap().proof().clone();
        let process = Process::read(std::process::id() as i32).unwrap();
        let identity = chat::runtime::ActorIdentity {
            instance: uuid::Uuid::new_v4(),
            record: Record {
                version: 4,
                build: None,
                kind: Kind::Native,
                application: crate::instance::APPLICATION_ID.into(),
                boot: proof.boot_id,
                terminal: source.id,
                holder: process.clone(),
                root: process,
            },
        };
        host.lock().unwrap().install_model(
            super::super::super::model::Registry::at(&root.path().join("registry")).unwrap(),
        );
        assert!(matches!(
            enroll_pending_for_start(&host, &identity, &source, &[], Err("captured"), true),
            Err(SetupError::Invalid(_)),
        ));
        enroll_pending_for_start(&host, &identity, &source, &[], Err("captured"), false).unwrap();
        for conflict in 0..3 {
            let mut identity = identity.clone();
            let mut source = source.clone();
            let mut scope = Err("captured");
            match conflict {
                0 => identity.record.root.start += 1,
                1 => source.launch.recipe.arguments.push("--changed".into()),
                _ => scope = Err("another captured account"),
            }
            assert!(matches!(
                enroll_pending_for_start(&host, &identity, &source, &[], scope, false),
                Err(SetupError::Invalid(_)),
            ));
        }
        enroll_pending_for_start(&host, &identity, &source, &[], Err("captured"), true).unwrap();
        let registry = super::super::super::agent_execution::registry(&host).unwrap();
        let owner = registry
            .lock()
            .unwrap()
            .native_actor_ownership(identity.instance)
            .unwrap()
            .unwrap();
        assert_eq!(owner.owner(), &identity.record);
        assert_eq!(owner.source(), &source);
        assert_eq!(owner.scope(), Err("captured"));
        assert!(owner.is_pending());
    }

    #[test]
    fn cleanup_recovery_without_a_resident_owner_can_be_retried() {
        let root = tempfile::tempdir().unwrap();
        let (host, source, _) = pending(root.path());
        assert!(matches!(
            recover_cleanup_attachment_for_start(&host, &source),
            Err(SetupError::Unavailable(_)),
        ));
        assert!(host.lock().unwrap().chats.is_empty());
    }

    #[test]
    fn retained_publication_rejects_changed_source_before_private_operations() {
        let root = tempfile::tempdir().unwrap();
        let (host, mut source, _) = pending(root.path());
        let handle = Handle::fixture_failed_remote();
        let identity = handle.actor_identity().unwrap();
        source.id = identity.record.terminal;
        source.proof.as_mut().unwrap().boot_id = identity.record.boot.clone();
        for conflict in 0..3 {
            let mut changed = source.clone();
            match conflict {
                0 => changed.id = TerminalId::new(),
                1 => changed.proof = None,
                _ => changed.launch.provider = Provider::Codex,
            }
            assert!(matches!(
                publish_authenticated_source_for_start(
                    &host,
                    &handle,
                    changed,
                    &[],
                    None,
                    None,
                    Publication::Install,
                ),
                Err(SetupError::Invalid(_)),
            ));
        }
        assert!(host.lock().unwrap().chats.is_empty());
    }

    #[test]
    fn exited_attachment_keeps_source_conflicts_blocked_and_registry_failure_retryable() {
        let root = tempfile::tempdir().unwrap();
        let (host, mut source, _) = pending(root.path());
        let handle = Handle::fixture_failed_remote();
        let identity = handle.actor_identity().unwrap();
        source.id = identity.record.terminal;
        host.lock().unwrap().chats.insert(
            source.id,
            super::super::Record {
                session: source.clone(),
                handle: handle.clone(),
                execution_instance: identity.instance,
                console_recipe: None,
                paired_terminal: None,
                source_process: None,
                subscribers: Default::default(),
                observations: Default::default(),
                completion_observed: false,
                returning_terminal: false,
            },
        );
        let mut changed = source.clone();
        changed
            .launch
            .recipe
            .arguments
            .push("--changed-source".into());
        assert!(matches!(
            finish_exited_attachment_for_start(&host, &changed),
            Err(SetupError::Invalid(_)),
        ));
        // This exact owner reaches the operational registry lookup. The
        // fixture has no registry, so cleanup remains pending and retryable.
        assert!(matches!(
            finish_exited_attachment_for_start(&host, &source),
            Err(SetupError::Unavailable(_)),
        ));
        let state = host.lock().unwrap();
        assert!(state.chats[&source.id].handle.same_attachment(&handle));
        assert_eq!(state.chats[&source.id].session, source);
        assert!(state.chat_spawning.is_empty());
    }
}

/// Persist cleanup authority while the inspected source and captured scope are
/// still authenticated, before installation or a new attachment can retire the
/// previous epoch. Pending is never an input or history-resume grant.
fn enroll_pending_for_start(
    host: &SharedHost,
    identity: &chat::runtime::ActorIdentity,
    issued: &Session,
    aliases: &[Session],
    scope: Result<&crate::account::scope::LaunchAccountScope, &str>,
    installed: bool,
) -> Result<(), chat::protocol::SetupError> {
    use chat::protocol::SetupError;
    let registry =
        super::super::agent_execution::registry(host).map_err(SetupError::Unavailable)?;
    let mut registry = registry.lock().unwrap_or_else(|p| p.into_inner());
    let existing = registry.native_actor_ownership_for_start(identity.instance)?;
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
    registry
        .register_pending_native_actor_ownership(identity.instance, &identity.record, source, scope)
        .map_err(SetupError::Unavailable)
}

#[derive(Clone, Copy)]
pub(super) enum Publication<'a> {
    Install,
    Console(&'a crate::host::model::console_relaunch::ConsoleRelaunch),
    Close,
}

#[cfg(test)]
pub(super) fn publish_authenticated_source(
    host: &SharedHost,
    handle: &Handle,
    source: Session,
    retained: &[crate::agent_execution::HostedSession],
    console: Option<crate::model::LaunchRecipe>,
    pair: Option<crate::terminal::holder::Record>,
    publication: Publication<'_>,
) -> Result<Session, String> {
    publish_authenticated_source_for_start(
        host,
        handle,
        source,
        retained,
        console,
        pair,
        publication,
    )
    .map_err(|failure| failure.to_string())
}

/// Commit normal installation before projecting ownership, then publish one
/// exact acknowledged tuple to Registry and return that same tuple to the HOST.
/// Close-only attachment keeps its existing ownership and never installs input.
/// Every caller supplies previously authenticated/issued aliases and revalidates
/// exact actor/pair ownership before exposing the returned record.
pub(super) fn publish_authenticated_source_for_start(
    host: &SharedHost,
    handle: &Handle,
    mut source: Session,
    retained: &[crate::agent_execution::HostedSession],
    console: Option<crate::model::LaunchRecipe>,
    pair: Option<crate::terminal::holder::Record>,
    publication: Publication<'_>,
) -> Result<Session, chat::protocol::SetupError> {
    use chat::protocol::SetupError;
    let identity = handle
        .actor_identity()
        .ok_or("Retained publication lost its authenticated actor identity")?;
    if source.id != identity.record.terminal || source.launch.provider != Provider::Claude {
        return Err("Pending publication changed its exact authenticated actor".into());
    }
    let install = !matches!(publication, Publication::Close);
    if let Publication::Console(intent) = publication
        && (handle.actor_identity().is_none_or(|identity| {
            identity.instance != intent.actor || identity.record != intent.owner
        }) || pair.as_ref() != intent.replacement_holder.as_ref())
    {
        return Err("Console publication lost its exact provider or replacement holder".into());
    }
    if install == handle.is_close_only() {
        return Err("Retained publication mode does not match its authenticated attachment".into());
    }
    let proof = source
        .proof
        .clone()
        .filter(HostSessionProof::is_complete)
        .ok_or("Pending publication lacks daemon proof")?;
    if identity.instance.is_nil()
        || identity.record.kind != crate::daemon::holder_registry::Kind::Native
        || identity.record.boot != proof.boot_id
    {
        return Err("Pending publication changed its exact authenticated actor".into());
    }
    if install {
        let inspection =
            Handle::inspect_remote(&identity.record, proof).map_err(SetupError::Unavailable)?;
        if inspection.identity != identity {
            return Err("Pending publication changed its exact authenticated actor".into());
        }
        enroll_pending_for_start(
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
        handle
            .commit_native_installation()
            .map_err(SetupError::Unavailable)?;
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
    handle
        .update_native_source(source.clone(), console, pair)
        .map_err(SetupError::Unavailable)?;
    if matches!(publication, Publication::Console(_)) {
        super::console_relaunch::checkpoint("pair").map_err(SetupError::Unavailable)?;
    }
    if install {
        let registry =
            super::super::agent_execution::registry(host).map_err(SetupError::Unavailable)?;
        let successor = crate::agent_execution::HostedSession::Chat(source.clone());
        let adopted = {
            let mut registry = registry.lock().unwrap_or_else(|p| p.into_inner());
            match publication {
                Publication::Console(intent) => registry
                    .adopt_authenticated_console_session(retained, &successor, intent)
                    .map_err(SetupError::Unavailable)?,
                _ => registry
                    .adopt_authenticated_agent_session(retained, &successor)
                    .map_err(SetupError::Unavailable)?,
            }
        };
        // Promotion follows the exact helper ACK and Registry transaction and
        // precedes any ordinary HOST input record becoming visible.
        register_ownership(host, handle, &source).map_err(SetupError::Unavailable)?;
        // The agent's record, and possibly its pane, now name the registered
        // successor; watching windows learn it from the edge.
        if adopted {
            super::super::agent_execution::announce(host, &registry);
        }
    }
    Ok(source)
}

#[cfg(test)]
pub(super) fn restore(
    host: &SharedHost,
    client: ClientId,
    requested: &Session,
    proof: &HostSessionProof,
    owner: crate::daemon::holder_registry::Record,
) -> Result<ServerResponse, String> {
    restore_actor(host, client, requested, proof, owner, true, None)
}

/// Reattach to the resident Claude reducer; never open provider stdin, reload
/// its transcript as a replacement, or reconstruct command/control UUIDs.
pub(super) fn restore_for_start(
    host: &SharedHost,
    client: ClientId,
    requested: &Session,
    proof: &HostSessionProof,
    owner: crate::daemon::holder_registry::Record,
) -> Result<ServerResponse, chat::protocol::SetupError> {
    restore_actor_for_start(host, client, requested, proof, owner, true, None)
}

/// An ordinary daemon exit closes each provider, then retires its helper once
/// the final outcomes are committed. A daemon that ends between the two (a
/// final commit failed, an attention wait timed out, or it was killed) leaves
/// the helper closed but still owning the conversation. Adopted as a live
/// attachment, that helper strands its pane: a closed actor cannot resolve a
/// recovery source, and its owner blocks a resume. Finish the same exit
/// instead, through the checks the exiting daemon would have made, so the
/// conversation reopens from saved history as after any other stop.
///
/// Returns false, changing nothing, unless the helper reports its provider
/// exited and its retirement is still pending. The close itself is proved
/// after attaching, since the final publication names the attaching daemon.
pub(in crate::host) fn finish_interrupted_exit(
    host: &SharedHost,
    owner: &crate::daemon::holder_registry::Record,
) -> Result<bool, String> {
    finish_interrupted_exit_for_start(host, owner, None).map_err(|failure| failure.to_string())
}

fn finish_interrupted_exit_for_start(
    host: &SharedHost,
    owner: &crate::daemon::holder_registry::Record,
    requested: Option<&Session>,
) -> Result<bool, chat::protocol::SetupError> {
    use chat::protocol::SetupError;
    let proof = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .proof()
        .clone();
    let inspection =
        Handle::inspect_remote(owner, proof.clone()).map_err(SetupError::Unavailable)?;
    if &inspection.identity.record != owner
        || requested.is_some_and(|requested| {
            requested != &inspection.source && !inspection.previous_sources.contains(requested)
        })
    {
        return Err("The retained actor does not recognize this exact cleanup descriptor".into());
    }
    if inspection.source.launch.provider != Provider::Claude
        || inspection.state.owns_work
        || !inspection.state.close.exited
        || !matches!(
            Handle::inspect_retained_retirement(owner, &proof).map_err(SetupError::Unavailable)?,
            RetirementInspection::Running
        )
    {
        return Ok(false);
    }
    // The explicit Close path, ending the agent as a stop. Attaching for
    // cleanup republishes the closed snapshot under this daemon, and
    // finish_close then proves that exact descriptor closed. A failure keeps
    // the close-only attachment, which an explicit Close retries.
    close_with_target(
        host,
        &inspection.source,
        Some(owner.clone()),
        None,
        None,
        Ending::Stopped,
    )
    .map_err(SetupError::Unavailable)?;
    Ok(true)
}

/// [`finish_interrupted_exit`] for the helper that owns `session`, when no
/// attachment holds it, for an owner startup discovery did not finish.
pub(in crate::host) fn finish_interrupted_exit_of_for_start(
    host: &SharedHost,
    session: &Session,
) -> Result<bool, chat::protocol::SetupError> {
    use crate::daemon::holder_registry::{self as registry, Kind};
    use chat::protocol::SetupError;
    if session.launch.provider != Provider::Claude
        || host
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .chats
            .contains_key(&session.id)
    {
        return Ok(false);
    }
    let directory = registry::directory(Kind::Native).map_err(SetupError::Unavailable)?;
    match registry::recovery_candidate(&registry::socket(&directory, session.id), Kind::Native)
        .map_err(SetupError::Unavailable)?
    {
        Some(owner) if Handle::is_remote_record(&owner) => {
            finish_interrupted_exit_for_start(host, &owner, Some(session))
        }
        _ => Ok(false),
    }
}

/// Ends the exact hosted actor of `source` whose provider exited while this
/// daemon kept running, as a stop rather than a close: its agent keeps its
/// pane and channel and its final attention stays, as after a daemon stop.
/// The verified retirement this leaves is what lets a resume prepare a
/// successor on the same conversation; nothing the actor held is resubmitted.
///
/// Returns false, changing nothing, unless `source` names this daemon's
/// attachment, its actor is retained and its provider has exited. A provider
/// that is still running is never ended here. An exit the provider did not
/// choose, such as a kill, leaves a failure that keeps the actor owning work
/// until someone answers it; resuming is that answer.
pub(in crate::host) fn finish_exited_attachment_for_start(
    host: &SharedHost,
    source: &Session,
) -> Result<bool, chat::protocol::SetupError> {
    let target = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        let Some(record) = state.chats.get(&source.id) else {
            return Ok(false);
        };
        if source.launch.provider != Provider::Claude
            || source.proof.as_ref() != Some(state.proof())
            || record.handle.actor_identity().is_none()
            || !record.handle.is_completed()
            || record.handle.is_close_only()
            || record.returning_terminal
        {
            return Ok(false);
        }
        if !recognizes_cleanup_source(&state, record, source) {
            return Err(
                "The retained actor does not recognize this exact cleanup descriptor".into(),
            );
        }
        super::CloseTarget::from_record(record)
    };
    close_with_target(host, source, None, Some(&target), None, Ending::Stopped)
        .map_err(chat::protocol::SetupError::Unavailable)?;
    Ok(true)
}

/// Whether `issued`'s paired CLI view has no worker here and its original
/// holder left an exact close receipt, so cleanup may proceed without it.
fn paired_view_proved_closed(
    host: &SharedHost,
    issued: &Session,
    pair: Option<&crate::terminal::holder::Record>,
    proof: &HostSessionProof,
) -> Result<bool, String> {
    let (Some(terminal), Some(pair)) = (&issued.terminal, pair) else {
        return Ok(false);
    };
    if terminal.id != pair.terminal
        || pair.terminal != issued.id
        || host
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .terminals
            .contains_key(&terminal.id)
    {
        return Ok(false);
    }
    let original = issued
        .proof
        .as_ref()
        .ok_or("The retained Claude pair lacks its issuing daemon proof")?;
    let role = crate::daemon::holder_registry::OriginalRole::authenticated(pair, original, proof)?;
    crate::terminal::holder::cleanup::confirmed_owner_with_role(pair, proof, role.as_ref())
}

/// An ordinary exit ends a retained actor's paired CLI view with it. Closing
/// the view through its holder leaves the close receipt a later resume needs
/// to prove the view released the conversation; the terminal shutdown that
/// follows would only hang it up, which proves nothing to the next daemon.
pub(in crate::host) fn close_paired_view(host: &SharedHost, id: TerminalId) -> Result<(), String> {
    let (terminal, worker, proof) = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        let Some(record) = state.chats.get(&id) else {
            return Ok(());
        };
        let (Some(terminal), Some(worker)) = (&record.session.terminal, &record.paired_terminal)
        else {
            return Ok(());
        };
        if record.handle.actor_identity().is_none() || worker.holder_identity().is_none() {
            return Ok(());
        }
        if !state.terminals.get(&terminal.id).is_some_and(|current| {
            current.handle.same_worker(worker)
                && super::matches_paired_terminal(&current.session, terminal, state.proof())
        }) {
            return Err(
                "The paired CLI view changed before shutdown; its owner was preserved".into(),
            );
        }
        (terminal.clone(), worker.clone(), state.proof().clone())
    };
    worker.close_with_receipt(&terminal, &proof)
}

/// A failed cleanup attachment is not a user request to return to a terminal.
/// Restore its still-running owner through normal installation, retaining the
/// old cleanup record until the helper and durable registry acknowledge it.
pub(super) fn recover_cleanup_attachment(
    host: &SharedHost,
    requested: &Session,
) -> Result<(), String> {
    recover_cleanup_attachment_for_start(host, requested).map_err(|failure| failure.to_string())
}

pub(super) fn recover_cleanup_attachment_for_start(
    host: &SharedHost,
    requested: &Session,
) -> Result<(), chat::protocol::SetupError> {
    use chat::protocol::SetupError;
    let setup_failure = |failure: ProtocolError| {
        if failure.retryable {
            SetupError::Unavailable(failure.message)
        } else {
            SetupError::Invalid(failure.message)
        }
    };
    super::super::account_lifecycle::ensure_idle(host).map_err(setup_failure)?;
    let (target, identity, proof, _reservation) = {
        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
        let record = state
            .chats
            .get(&requested.id)
            .ok_or_else(|| SetupError::Unavailable("Cleanup owner disappeared".into()))?;
        if !recognizes_cleanup_source(&state, record, requested)
            || !record.handle.is_close_only()
            || record.session.launch.provider != Provider::Claude
        {
            return Err("This conversation is not a running recoverable cleanup attachment".into());
        }
        if record.handle.is_completed()
            || state.shutting_down
            || state.chat_handoffs.contains_key(&requested.id)
            || state.native_closing.contains_key(&requested.id)
        {
            return Err(SetupError::Unavailable(
                "This conversation is not a running recoverable cleanup attachment".into(),
            ));
        }
        if let Some(pending) = state.chat_spawning.get(&requested.id) {
            return Err(if pending == requested {
                SetupError::Unavailable("Chat is already starting or closing".into())
            } else {
                SetupError::Invalid("Pending chat has a different exact launch".into())
            });
        }
        let target = super::CloseTarget::from_record(record);
        let identity = record
            .handle
            .actor_identity()
            .ok_or("Cleanup lost its exact helper")?;
        let proof = state.proof().clone();
        let reservation =
            capacity::Reservation::closing(host, &mut state, requested).map_err(setup_failure)?;
        (target, identity, proof, reservation)
    };
    restore_actor_for_start(
        host,
        0,
        requested,
        &proof,
        identity.record,
        true,
        Some(&target),
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
    restore_actor(host, 0, requested, proof, owner, false, None)
}

fn restore_actor(
    host: &SharedHost,
    client: ClientId,
    requested: &Session,
    proof: &HostSessionProof,
    owner: crate::daemon::holder_registry::Record,
    publish_snapshot: bool,
    replacing: Option<&super::CloseTarget>,
) -> Result<ServerResponse, String> {
    restore_actor_for_start(
        host,
        client,
        requested,
        proof,
        owner,
        publish_snapshot,
        replacing,
    )
    .map_err(|error| error.to_string())
}

fn restore_actor_for_start(
    host: &SharedHost,
    client: ClientId,
    requested: &Session,
    proof: &HostSessionProof,
    owner: crate::daemon::holder_registry::Record,
    publish_snapshot: bool,
    replacing: Option<&super::CloseTarget>,
) -> Result<ServerResponse, chat::protocol::SetupError> {
    let result = restore_actor_inner(
        host,
        client,
        requested,
        proof,
        owner.clone(),
        publish_snapshot,
        replacing,
    );
    if let Err(error) = &result {
        let console = publish_snapshot
            && Handle::inspect_remote(&owner, proof.clone()).is_ok_and(|inspection| {
                inspection.state.owns_work
                    && !inspection.state.completed
                    && !inspection.state.close.closing
                    && !inspection.state.close.exited
                    && inspection
                        .execution_console
                        .as_ref()
                        .and_then(crate::agent_execution::console_agent)
                        .is_some()
            });
        let pending = super::super::agent_execution::registry(host)
            .and_then(|registry| {
                registry
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .has_console_relaunch(requested.id)
            })
            .unwrap_or(true);
        if publish_snapshot && (console || pending) {
            host.lock().unwrap_or_else(|p| p.into_inner()).retain_startup_recovery(format!("The retained agent's console could not reconnect; its provider and accepted work remain owned: {error}"));
        }
    }
    result
}

fn restore_actor_inner(
    host: &SharedHost,
    client: ClientId,
    requested: &Session,
    proof: &HostSessionProof,
    owner: crate::daemon::holder_registry::Record,
    publish_snapshot: bool,
    replacing: Option<&super::CloseTarget>,
) -> Result<ServerResponse, chat::protocol::SetupError> {
    use chat::protocol::SetupError;
    if owner.terminal != requested.id
        || requested.launch.provider != Provider::Claude
        || !Handle::is_remote_record(&owner)
    {
        return Err("The retained Claude actor identity changed during inspection".into());
    }
    if host.lock().unwrap_or_else(|p| p.into_inner()).shutting_down {
        return Err(SetupError::Unavailable(
            "Daemon ownership changed before retained Claude registration; the provider remains owned by its holder".into(),
        ));
    }
    // The private RPC still reports operational failures as opaque strings.
    // Preserve Retry for those failures; classify only structured identities
    // observed below, never words in a helper's diagnostic.
    let inspection =
        Handle::inspect_remote(&owner, proof.clone()).map_err(SetupError::Unavailable)?;
    if inspection.identity.record != owner
        || inspection.source.id != requested.id
        || inspection.source.launch.provider != Provider::Claude
    {
        return Err("The retained Claude actor identity changed during inspection".into());
    }
    if requested != &inspection.source && !inspection.previous_sources.contains(requested) {
        return Err("Retained actor source/account identity does not match this chat".into());
    }
    if let Some(target) = replacing {
        target.require_current(host).map_err(|error| {
            let state = host.lock().unwrap_or_else(|p| p.into_inner());
            if state.shutting_down || state.chat_handoffs.contains_key(&requested.id) {
                SetupError::Unavailable(error.message)
            } else {
                SetupError::Invalid(error.message)
            }
        })?;
        if target.handle.actor_identity().as_ref() != Some(&inspection.identity) {
            return Err("The cleanup helper no longer owns a running provider; retirement evidence was preserved".into());
        }
        if !inspection.state.installed
            || inspection.state.completed
            || !inspection.state.owns_work
            || inspection.state.close.closing
            || inspection.state.close.exited
        {
            return Err(SetupError::Unavailable("The cleanup helper no longer owns a running provider; retirement evidence was preserved".into()));
        }
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
        )
        .map_err(SetupError::Unavailable)?;
    } else {
        enroll_pending_for_start(
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
        Handle::adopt_remote(&owner, requested.clone(), proof.clone(), notify)
            .map_err(SetupError::Unavailable)?
    } else {
        Handle::adopt_remote_for_close(&owner, requested.clone(), proof.clone(), notify)
            .map_err(SetupError::Unavailable)?
    };
    let identity = handle
        .actor_identity()
        .ok_or("The retained actor did not provide its stable identity")?;
    if identity != inspection.identity {
        return Err("The retained actor changed while attaching".into());
    }
    let mut session = requested.clone();
    session.proof = Some(proof.clone());
    // A presentation alias predating the console cannot release its current
    // exact paired worker. Only authenticated helper metadata names that pair.
    session.terminal = inspection.source.terminal.clone();
    let replacement = if publish_snapshot
        && !inspection.state.completed
        && inspection.state.owns_work
        && !inspection.state.close.closing
        && !inspection.state.close.exited
    {
        super::console_relaunch::prepare(
            host,
            &handle,
            requested,
            proof,
            &inspection.source,
            &inspection.previous_sources,
            inspection.execution_console.as_ref(),
            inspection.paired_holder.as_ref(),
        )
        .map_err(SetupError::Unavailable)?
    } else {
        None
    };
    if let Some(replacement) = &replacement {
        session.terminal = Some(Box::new(replacement.terminal.clone()));
    }
    // A paired CLI view whose worker is gone may have been closed by the
    // exit that left this helper. Only its original holder's exact close
    // receipt lets cleanup proceed without it; no terminal worker is touched.
    let pair_closed = !publish_snapshot
        && paired_view_proved_closed(
            host,
            &inspection.source,
            inspection.paired_holder.as_ref(),
            proof,
        )
        .map_err(SetupError::Unavailable)?;
    let paired = if let Some(replacement) = &replacement {
        Some(replacement.handle.clone())
    } else {
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
            // The helper keeps naming the closed pair, so its retirement
            // still carries the evidence a resume rechecks.
            (Some(terminal), Some(_))
                if pair_closed && !state.terminals.contains_key(&terminal.id) =>
            {
                terminal.continuity = crate::model::TerminalContinuity::Live {
                    host_session_id: proof.host_session_id.clone(),
                    boot_id: proof.boot_id.clone(),
                    host_incarnation: proof.host_incarnation.clone(),
                };
                None
            }
            (Some(terminal), Some(owner)) => {
                let current = state.terminals.get(&terminal.id).ok_or_else(|| SetupError::Unavailable(
                    "The retained Claude console's exact worker could not be recovered; provider ownership was preserved".into(),
                ))?;
                if current.handle.holder_identity() != Some(owner)
                    || current.session.launch != terminal.launch
                    || current.session.owned_codex_listener != terminal.owned_codex_listener
                {
                    return Err("The retained Claude console's exact worker could not be recovered; provider ownership was preserved".into());
                }
                **terminal = state
                    .native_terminal_projection(terminal.id)
                    .map_err(SetupError::Unavailable)?;
                if terminal.continuity.live_proof().as_ref() != Some(proof) {
                    return Err(SetupError::Unavailable(
                        "The retained Claude console belongs to a different daemon owner".into(),
                    ));
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
    if let Some(replacement) = &replacement {
        sources.extend(replacement.sources.clone());
    }
    sources.sort_by_key(|source| serde_json::to_vec(source).unwrap_or_default());
    sources.dedup();
    session = publish_authenticated_source_for_start(
        host,
        &handle,
        session,
        &sources
            .iter()
            .cloned()
            .map(crate::agent_execution::HostedSession::Chat)
            .collect::<Vec<_>>(),
        replacement
            .as_ref()
            .map(|r| r.terminal.launch.clone())
            .or_else(|| inspection.execution_console.clone()),
        replacement
            .as_ref()
            .and_then(|r| r.intent.replacement_holder.clone())
            .or_else(|| inspection.paired_holder.clone()),
        if let Some(replacement) = &replacement {
            Publication::Console(&replacement.intent)
        } else if publish_snapshot {
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
        console_recipe: replacement
            .as_ref()
            .map(|r| r.terminal.launch.clone())
            .or(inspection.execution_console),
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
        session_snapshot(&record.session, &record.handle).map_err(SetupError::Unavailable)?
    } else {
        // Cleanup adopts ownership without reading a presentation window.
        ServerResponse::Accepted
    };
    let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
    let pair_unchanged = match (&record.session.terminal, &paired) {
        (None, None) => true,
        (Some(terminal), None) => pair_closed && !state.terminals.contains_key(&terminal.id),
        (Some(terminal), Some(handle)) => {
            state.terminals.get(&terminal.id).is_some_and(|current| {
                current.handle.same_worker(handle)
                    && state.native_terminal_projection(terminal.id).as_ref() == Ok(terminal)
            })
        }
        _ => false,
    };
    if state.shutting_down || state.proof() != proof {
        return Err(SetupError::Unavailable("Daemon ownership changed before retained Claude registration; the provider remains owned by its holder".into()));
    }
    if replacing.is_none()
        && state.chats.get(&id).is_some_and(|current| {
            current.session == record.session
                && current.execution_instance == record.execution_instance
                && current.handle.same_actor(&handle)
        })
    {
        return Err(SetupError::Unavailable("Daemon ownership changed before retained Claude registration; the provider remains owned by its holder".into()));
    }
    if !pair_unchanged
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
    state.chat_added(id);
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
    close_with_target(host, requested, discovered, None, None, Ending::Closed)
}

/// A hosted close must retain its exact admission capability across cleanup.
pub(super) fn close_captured(
    host: &SharedHost,
    requested: &Session,
    target: &super::CloseTarget,
    closing: Option<&super::capacity::Reservation>,
) -> Result<(), String> {
    close_with_target(host, requested, None, Some(target), closing, Ending::Closed)
}

/// What a retained cleanup ends besides the helper and its provider.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Ending {
    /// The person closed the conversation: its agent and attention close too.
    Closed,
    /// A daemon stop ended it: the agent keeps its pane and channel, and its
    /// final attention stays, so the conversation resumes in place.
    Stopped,
}

impl Ending {
    fn preflight(self, host: &SharedHost, source: &Session) -> Result<(), String> {
        match self {
            Self::Closed => super::super::agent_execution::preflight_retirement(host, source),
            Self::Stopped => Ok(()),
        }
    }

    fn end(self, host: &SharedHost, source: &Session) -> Result<(), String> {
        match self {
            Self::Closed => super::super::agent_execution::retired(host, source),
            Self::Stopped => super::super::agent_execution::stopped(host, source),
        }
    }
}

/// The same issued, observed or authenticated upgrade alias accepted by cleanup.
fn recognizes_cleanup_source(
    state: &HostState,
    record: &super::Record,
    requested: &Session,
) -> bool {
    let mut observed = record.session.clone();
    observed.launch.session_id = record.handle.session_id();
    observed.native_continuity = record.handle.native_continuity();
    record.session == *requested
        || observed == *requested
        || state
            .upgrade_chats
            .get(&requested.id)
            .is_some_and(|aliases| aliases.iter().any(|alias| alias.source == *requested))
}

fn close_with_target(
    host: &SharedHost,
    requested: &Session,
    discovered: Option<crate::daemon::holder_registry::Record>,
    expected: Option<&super::CloseTarget>,
    closing: Option<&super::capacity::Reservation>,
    ending: Ending,
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
        if state.shutting_down
            || closing.map_or_else(
                || state.chat_spawning.contains_key(&requested.id),
                |closing| !closing.holds(&state, requested.id),
            )
            || state.chat_handoffs.contains_key(&requested.id)
            || state.native_closing.contains_key(&requested.id)
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
        let exited = match cleanup_retirement(host, requested, &proof, discovered.as_ref())? {
            CleanupRetirement::Verified(verified) => {
                return close_verified_unattached(host, requested, &proof, verified, ending);
            }
            CleanupRetirement::None => None,
            CleanupRetirement::ExitedUnproved => {
                Some("The retained cleanup helper exited without verified retirement")
            }
        };
        // An actor that died - killed, crashed, out of memory - leaves no
        // verified retirement and nothing to attach. A person's Close settles
        // once nothing of it can still run; a daemon stop keeps the refusal.
        if ending == Ending::Closed {
            match &discovered {
                // Its helper is gone but something of it runs on, which
                // nothing can now be asked to stop.
                Some(owner)
                    if !owner.holder.is_current()
                        && !crate::chat::runtime::retained_actor_abandoned(
                            &owner.holder,
                            &owner.root,
                        )? =>
                {
                    return Err(super::lost_execution_refusal(
                        requested,
                        super::Holds::Running,
                    ));
                }
                Some(owner) if !chat::native_holder::retire_abandoned(owner)? => {}
                _ => return super::settle_lost_execution(host, requested, &proof),
            }
        }
        if let Some(exited) = exited {
            return Err(exited.into());
        }
        let owner = discovered.as_ref().ok_or("No authenticated retained Claude actor is available; its saved descriptor was preserved")?;
        if !Handle::is_remote_record(owner) {
            return Err(
                "The discovered native holder does not support this retained Claude protocol"
                    .into(),
            );
        }
        restore_actor(host, 0, requested, &proof, owner.clone(), false, None)?;
    }
    let (handle, source, paired, target) = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        let record = state
            .chats
            .get(&requested.id)
            .ok_or("The retained actor changed during cleanup admission")?;
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
        if !recognizes_cleanup_source(&state, record, requested) {
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
                if observed != **terminal {
                    return Err(
                        "The paired terminal descriptor changed; its replacement was preserved"
                            .into(),
                    );
                }
                Some(((**terminal).clone(), current.handle.clone()))
            }
            // Cleanup attached without a worker for a view proved closed; the
            // helper's pair is rechecked below before anything is ended.
            (Some(terminal), None)
                if record.handle.is_close_only() && !state.terminals.contains_key(&terminal.id) =>
            {
                None
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
    ending.preflight(host, &source)?;
    // Authenticate all managed execution aliases before a destructive effect.
    let registry = super::super::agent_execution::registry(host)?;
    let sources = registry
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .retained_agent_sources(source.id)?;
    let identity = handle
        .actor_identity()
        .ok_or("Retained cleanup lost its actor identity")?;
    let (issued, lineage, scope, pair_holder, retired) =
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
                    inspection.paired_holder,
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
                    receipt.paired_holder.clone(),
                    Some(verified),
                )
            }
            RetirementInspection::ExitedUnproved => {
                // The resident's actor died without its retirement. As with
                // no resident, a person's Close settles once nothing of it
                // can still run, and then the resident goes too.
                if ending != Ending::Closed {
                    return Err(RETIREMENT_UNPROVED.into());
                }
                if !chat::native_holder::retire_abandoned(&identity.record)? {
                    // Its helper is gone but something of it runs on; the
                    // pane names it and offers to end it.
                    let holds = super::Holds::Running;
                    return Err(super::lost_execution_refusal(&source, holds));
                }
                // The actor's roster cannot see a paired console, which runs
                // outside it. That console's own worker still answers an
                // exact close, and its receipt proves the console ended; the
                // roster proves the rest.
                let actor = Session {
                    terminal: None,
                    ..source.clone()
                };
                // Everything that can still refuse comes before the console
                // is closed, so a refused Close keeps the pane as it was.
                super::refuse_scheduled_agent(host, &actor)?;
                if let Some(holds) = super::what_holds(host, &actor, &proof)? {
                    return Err(super::lost_execution_refusal(&source, holds));
                }
                if let Some((terminal, worker)) = &paired {
                    target
                        .require_current(host)
                        .map_err(|error| error.message)?;
                    close_paired_worker(host, terminal, worker, &proof)?;
                }
                super::settle_lost_execution(host, &actor, &proof)?;
                let paired = paired.as_ref();
                return remove_closed_resident(host, &proof, &source, &target, paired, ending);
            }
        };
    verify_close_ownership(
        host,
        &identity,
        &issued,
        &lineage,
        scope.as_ref().map_err(String::as_str),
    )?;
    if paired.is_none()
        && source.terminal.is_some()
        && !paired_view_proved_closed(host, &issued, pair_holder.as_ref(), &proof)?
    {
        return Err(
            "The retained Claude pair has no exact close proof; no pair was released".into(),
        );
    }
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
        if ending == Ending::Closed {
            registry
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .preflight_agent_retirement(retained)?;
        }
    }
    target
        .require_current(host)
        .map_err(|error| error.message)?;
    if retired.is_none() {
        handle.close()?;
    }
    target
        .require_current(host)
        .map_err(|error| error.message)?;
    if let Some((terminal, worker)) = &paired {
        close_paired_worker(host, terminal, worker, &proof)?;
    }
    target
        .require_current(host)
        .map_err(|error| error.message)?;
    if let Some(verified) = retired {
        publish_verified_retirement(host, &handle, &source, verified)?;
    } else {
        finish_close(host, &handle, &source)?;
    }
    target
        .require_current(host)
        .map_err(|error| error.message)?;
    // Keep the closed record retryable until durable process retirement succeeds.
    ending.end(host, &source)?;
    remove_closed_resident(host, &proof, &source, &target, paired.as_ref(), ending)
}

/// Closes a retained actor's paired console through its own worker, unless
/// that worker was replaced.
fn close_paired_worker(
    host: &SharedHost,
    terminal: &crate::model::TerminalSession,
    worker: &crate::terminal::TerminalHandle,
    proof: &HostSessionProof,
) -> Result<(), String> {
    let current = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        state.terminals.get(&terminal.id).is_some_and(|current| {
            current.handle.same_worker(worker)
                && super::matches_paired_terminal(&current.session, terminal, state.proof())
        })
    };
    if !current {
        return Err(
            "The paired worker changed before retained cleanup; replacement was preserved".into(),
        );
    }
    worker.close_with_receipt(terminal, proof)
}

/// Removes a resident whose retained actor is closed or proved gone, unless
/// its record, its paired worker or the daemon's proof changed meanwhile.
fn remove_closed_resident(
    host: &SharedHost,
    proof: &HostSessionProof,
    source: &Session,
    target: &super::CloseTarget,
    paired: Option<&(
        crate::model::TerminalSession,
        crate::terminal::TerminalHandle,
    )>,
    ending: Ending,
) -> Result<(), String> {
    let removed = {
        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
        if state.proof() != proof
            || !state
                .chats
                .get(&source.id)
                .is_some_and(|record| target.matches(record))
            || paired.is_some_and(|(terminal, worker)| {
                state
                    .terminals
                    .get(&terminal.id)
                    .is_none_or(|current| !current.handle.same_worker(worker))
            })
        {
            return Err(
                "Retained ownership changed during cleanup; replacement records were preserved"
                    .into(),
            );
        }
        if let Some((terminal, _)) = paired {
            state.retire_terminal(terminal.id);
        }
        let removed = state.chats.remove(&source.id);
        // Ordinary Close may finish a previously failed initialization too.
        // Clear only the startup captured by this fully retired attachment;
        // a replacement owner or an unproved teardown retains its reservation.
        if removed.as_ref().is_some_and(|record| {
            state
                .native_actor_startups
                .get(&source.id)
                .is_some_and(|pending| {
                    pending
                        .failed_close
                        .as_ref()
                        .is_some_and(|failed| failed.matches(record))
                })
        }) {
            state.native_actor_startups.remove(&source.id);
        }
        state.native_adoption_failures.clear(source.id);
        if ending == Ending::Closed
            && let Some(notifications) = &state.notifications
        {
            notifications.acknowledge_session(source.id);
        }
        state.inventory_generation += 1;
        removed
    };
    drop(removed);
    super::super::state::retire_unattached_upgrade_chat(host, source.id);
    Ok(())
}

/// Why a resident's Close waits when its helper exited without the receipt.
const RETIREMENT_UNPROVED: &str = "The closed helper lacks its exact semantic-retirement receipt";

/// What this pane's durable owners record about its retained actor's end.
enum CleanupRetirement {
    /// An owner's verified retirement names this exact descriptor.
    Verified(VerifiedRetirement),
    /// No owner has ended, or none is registered.
    None,
    /// An owner's helper exited without its verified retirement.
    ExitedUnproved,
}

/// Discovery disappears at retirement, before the durable HOST transaction.
/// Consult only this pane's durable owners; absence never authorizes relaunch.
fn cleanup_retirement(
    host: &SharedHost,
    requested: &Session,
    proof: &HostSessionProof,
    discovered: Option<&crate::daemon::holder_registry::Record>,
) -> Result<CleanupRetirement, String> {
    let registry = super::super::agent_execution::registry(host)?;
    let mut after = None;
    let mut matched = None;
    loop {
        let page = registry
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .native_actor_ownerships(after, 128)?;
        if page.is_empty() {
            return Ok(matched.map_or(CleanupRetirement::None, CleanupRetirement::Verified));
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
                    return Ok(CleanupRetirement::ExitedUnproved);
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
    ending: Ending,
) -> Result<(), String> {
    let receipt = verified.receipt();
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
            if state.shutting_down
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
        if ending == Ending::Closed {
            registry
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .preflight_agent_retirement(retained)?;
        }
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
            ending.end(host, source)?;
        }
    }
    {
        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
        if state.shutting_down
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
        if ending == Ending::Closed
            && let Some(notifications) = &state.notifications
        {
            notifications.acknowledge_session(requested.id);
        }
        state.inventory_generation += 1;
    }
    super::super::state::retire_unattached_upgrade_chat(host, requested.id);
    Ok(())
}

/// These IDs came from the authenticated actor's journal at attachment, not
/// from a user pane, busy bit, or a prior daemon's accepted dispatch record.
/// Each run is paired with the attachment's resident source, its confirmed
/// projection, and the descriptor its agent is bound to when that names the
/// same process. `persist_identity` rebinds the agent to the projection of the
/// conversation an outcome followed before the census row and the outcome
/// commit; a failure or restart in between leaves the agent on that projection,
/// and its runs stay protected for the helper's outcome.
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
    let registry = super::super::agent_execution::registry(host).ok();
    actors
        .into_iter()
        .flat_map(|(handle, source)| {
            let bound = registry.as_ref().and_then(|registry| {
                match registry
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .agent_for_session(source.id)
                    .ok()??
                    .session?
                {
                    crate::agent_execution::HostedSession::Chat(bound)
                        if crate::agent_execution::same_chat_process(&bound, &source) =>
                    {
                        Some(bound)
                    }
                    _ => None,
                }
            });
            let mut aliases = vec![source.clone()];
            for alias in confirmed_projection(&handle, &source)
                .into_iter()
                .chain(bound)
            {
                if !aliases.contains(&alias) {
                    aliases.push(alias);
                }
            }
            let aliases = aliases
                .into_iter()
                .map(crate::agent_execution::HostedSession::Chat)
                .collect::<Vec<_>>();
            handle
                .retained_runs()
                .into_iter()
                .flat_map(move |run| aliases.clone().into_iter().map(move |alias| (run, alias)))
        })
        .collect()
}

/// Boot records each known loss before deciding which accepted runs remain
/// protected by the surviving actor. A loss that cannot commit yet, because its
/// confirmed conversation cannot be published, its owner cannot be
/// authenticated or the helper cannot be reached, holds back only that actor:
/// the helper keeps the loss unacknowledged, its affected runs stay retained
/// and so protected, and the outcome broker retries it as it retries any
/// uncommitted outcome. Recovery continues for every other actor and pane.
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
                    .map(|identity| SemanticTarget {
                        handle: record.handle.clone(),
                        source: record.session.clone(),
                        identity,
                        notifications: state.notifications.clone(),
                    })
            })
            .collect::<Vec<_>>()
    };
    let registry = super::super::agent_execution::registry(host)?;
    for target in actors {
        if let Err(error) = reconcile_startup_loss(host, &registry, &target) {
            tracing::warn!(id = %target.source.id, %error,
                "retained continuity loss is not committed yet; the helper retains it for the outcome broker");
            wake(host, target.source.id);
        }
    }
    Ok(())
}

fn reconcile_startup_loss(
    host: &SharedHost,
    registry: &super::super::agent_execution::SharedRegistry,
    target: &SemanticTarget,
) -> Result<(), String> {
    let SemanticTarget {
        handle,
        source,
        identity,
        ..
    } = target;
    handle.refresh_retained_state()?;
    let Some(loss) = handle.continuity_loss() else {
        return Ok(());
    };
    let execution = !loss.affected_runs.is_empty();
    if execution {
        // The loss settles runs; their conversation must be durable.
        persist_confirmed_identity(host, target, registry)?;
    }
    let mut execution_owner = None;
    let owner = capture_execution_owner(
        registry,
        handle,
        identity,
        source,
        execution,
        &mut execution_owner,
    )?;
    let confirmed = target.confirmed_projection();
    let required = target.required_identity(execution, None);
    let changed = {
        let mut registry = registry.lock().unwrap_or_else(|p| p.into_inner());
        require_execution_owner(&registry, owner, execution, confirmed.as_ref())?;
        require_published_identity(&registry, identity.instance, required.as_deref())?;
        registry.commit_native_actor_loss(
            identity.instance,
            &identity.record,
            source,
            &loss.affected_runs,
            &loss.reason,
            chat::now_ms().unwrap_or(0),
        )?
    };
    if changed {
        super::super::agent_execution::announce(host, registry);
    }
    target
        .require_current(host)
        .map_err(|_| "Retained startup ownership changed before loss acknowledgement")?;
    handle.ack_continuity_loss(loss.id)
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

/// Release the transcripts that this boot's retired actors keep in their final
/// receipts, and return the bytes released.
///
/// Only actors whose retirement the registry has committed are touched, and
/// the receipt itself stays: a resume later in this boot authenticates the
/// retirement through it. Runs beside the daemon, so it takes the host and
/// registry locks only to read the roster, never across file work.
pub(in crate::host) fn compact_retired_receipts(host: &SharedHost) -> Result<u64, String> {
    let proof = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        if state.shutting_down {
            return Ok(0);
        }
        state.proof().clone()
    };
    let registry = super::super::agent_execution::registry(host)?;
    let mut released = 0u64;
    let mut after = None;
    loop {
        let retired = {
            let registry = registry.lock().unwrap_or_else(|p| p.into_inner());
            let page = registry.native_actor_ownerships(after, 128)?;
            let Some(last) = page.last() else {
                return Ok(released);
            };
            after = Some(last.actor());
            page.into_iter()
                .filter(|ownership| ownership.owner().boot == proof.boot_id)
                .filter_map(
                    |ownership| match registry.native_actor_retirement(ownership.actor()) {
                        Ok(Some(_)) => Some(ownership.owner().clone()),
                        Ok(None) => None,
                        // One unreadable row keeps only its own receipt whole.
                        Err(error) => {
                            tracing::debug!(actor = %ownership.actor(), %error, "retirement unreadable; receipt kept whole");
                            None
                        }
                    },
                )
                .collect::<Vec<_>>()
        };
        for owner in retired {
            match Handle::compact_retired_receipt(&owner, &proof) {
                Ok(bytes) => released = released.saturating_add(bytes),
                Err(error) => {
                    tracing::debug!(terminal = %owner.terminal, %error, "retired receipt kept whole");
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
                // Every ordinary daemon stop leaves such a receipt, so finish
                // only the process end here: closing the agent or its pane
                // would undo the resumable stop. An explicit Close interrupted
                // between its receipt and registry commit stays open but
                // closable, since the receipt proves its process ended.
                super::super::agent_execution::stopped(host, retired.source())?;
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
                if pending
                    && !attached
                    && let Err(error) =
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
                            "Pending ownership changed during failed cleanup reconciliation".into(),
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
                            "Daemon ownership changed during pending cleanup reconciliation".into(),
                        );
                    }
                    let diagnostic = error.chars().take(1024).collect::<String>();
                    tracing::warn!(actor=%ownership.actor(), error=%diagnostic,
                            "Pending retained actor cleanup remains retryable; durable ownership was preserved");
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
            // The receipt proves the process ended. Record that like any other
            // stop, so its agent stays open and a resumed successor can adopt
            // it this boot, not only after the next startup.
            super::super::agent_execution::stopped(host, &receipt.source)?;
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
    let target = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        let record = state
            .chats
            .get(&requested.id)
            .filter(|record| record.handle.same_attachment(handle))
            .ok_or("Actor close ownership changed before final acknowledgement")?;
        SemanticTarget {
            handle: handle.clone(),
            source: record.session.clone(),
            identity: identity.clone(),
            notifications: notifications.clone(),
        }
    };
    // Shutdown may already have stopped the background broker, and the
    // provider can confirm its conversation while Close is in progress.
    // Flush that final identity before acknowledging outcomes or retirement.
    persist_confirmed_identity(host, &target, &registry)?;
    let loss = handle.continuity_loss();
    if let Some(loss) = &loss {
        let execution = !loss.affected_runs.is_empty();
        let owner = capture_execution_owner(
            &registry,
            handle,
            &identity,
            &source,
            execution,
            &mut execution_owner,
        )?;
        let confirmed = target.confirmed_projection();
        let required = target.required_identity(execution, None);
        let changed = {
            let mut registry = registry.lock().unwrap_or_else(|p| p.into_inner());
            require_execution_owner(&registry, owner, execution, confirmed.as_ref())?;
            require_published_identity(&registry, identity.instance, required.as_deref())?;
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
            let followed = entry.confirmed_session;
            let event = match entry.update {
                Update::Execution { run, update } => NativeActorEvent::Execution { run, update },
                Update::Attention(evidence) => NativeActorEvent::Attention(evidence),
                Update::AttentionContext { evidence, content } => {
                    NativeActorEvent::AttentionContext { evidence, content }
                }
                Update::AttentionCleared(clear) => NativeActorEvent::AttentionCleared(clear),
                _ => {
                    return Err(
                        "Closing actor journal contains an unsupported semantic event".into(),
                    );
                }
            };
            let execution = matches!(event, NativeActorEvent::Execution { .. });
            let required = target.required_identity(execution, followed.as_deref());
            // The page was read before the identity it follows was refreshed.
            target.publish_required(host, &registry, required.as_deref(), &mut execution_owner)?;
            let owner = capture_execution_owner(
                &registry,
                handle,
                &identity,
                &source,
                execution,
                &mut execution_owner,
            )?;
            let confirmed = target.required_projection(required.as_ref());
            let (changed, automation) = {
                let mut registry = registry.lock().unwrap_or_else(|p| p.into_inner());
                require_execution_owner(&registry, owner, execution, confirmed.as_ref())?;
                require_published_identity(&registry, identity.instance, required.as_deref())?;
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
            if let NativeActorEvent::AttentionCleared(clear) = event
                && !automation
            {
                commit_attention_clear(
                    notifications.as_ref(),
                    source.id,
                    identity.instance,
                    entry.sequence,
                    clear,
                    Duration::from_secs(10),
                )?;
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
    // An outcome that followed a conversation since reset republished it.
    persist_confirmed_identity(host, &target, &registry)?;
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
