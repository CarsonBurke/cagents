//! One coalescing semantic-outcome broker for all helper-owned actors. Provider
//! callbacks only wake it; neither callback return nor enqueueing acknowledges
//! a durable outcome. No RPC, SQLite transaction or journal wait holds HostState.
use super::*;
use crate::host::model::NativeActorEvent;
use std::{collections::BTreeSet, sync::{Condvar, Mutex}};

#[derive(Clone)]
pub(in crate::host) struct Broker(Arc<Inbox>);
struct Inbox {
    pending: Mutex<BTreeSet<TerminalId>>,
    wake: Condvar,
}

pub(super) fn wake(host: &SharedHost, id: TerminalId) {
    let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
    if state.shutting_down { return; }
    let broker = match state.native_actor_outcomes.clone() {
        Some(broker) => broker,
        None => {
            let inbox = Arc::new(Inbox { pending: Mutex::new(BTreeSet::new()), wake: Condvar::new() });
            let weak = Arc::downgrade(host);
            let worker = inbox.clone();
            if let Err(error) = thread::Builder::new().name("cagents-actor-outcomes".into()).spawn(move || run(weak, worker)) {
                tracing::error!(%error, "cannot start retained actor outcome broker; helper outbox remains unacknowledged");
                return;
            }
            let broker = Broker(inbox);
            state.native_actor_outcomes = Some(broker.clone());
            broker
        }
    };
    drop(state);
    broker.0.pending.lock().unwrap_or_else(|p| p.into_inner()).insert(id);
    broker.0.wake.notify_one();
}

fn run(host: Weak<HostLock>, inbox: Arc<Inbox>) {
    let mut retry = BTreeSet::new();
    let mut warned = BTreeSet::new();
    loop {
        let mut pending = inbox.pending.lock().unwrap_or_else(|p| p.into_inner());
        if pending.is_empty() {
            pending = inbox.wake.wait_timeout(pending, if retry.is_empty() { Duration::from_secs(5) } else { Duration::from_secs(1) }).unwrap_or_else(|p| p.into_inner()).0;
        }
        let mut work = std::mem::take(&mut *pending);
        drop(pending);
        work.append(&mut retry);
        let Some(host) = host.upgrade() else { return; };
        if host.lock().unwrap_or_else(|p| p.into_inner()).shutting_down { return; }
        for id in work {
            match drain(&host, id) {
                Ok(()) => { warned.remove(&id); }
                Err(error) => {
                    if warned.insert(id) { tracing::warn!(%id, %error, "retained actor outcome is not committed; helper retains it for retry"); }
                    retry.insert(id);
                }
            }
        }
    }
}

fn drain(host: &SharedHost, id: TerminalId) -> Result<(), String> {
    let (handle, source, identity, notifications) = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        let Some(record) = state.chats.get(&id) else { return Ok(()); };
        let Some(identity) = record.handle.actor_identity() else { return Ok(()); };
        (record.handle.clone(), record.session.clone(), identity, state.notifications.clone())
    };
    let registry = super::super::agent_execution::registry(host)?;
    // Always ask the HELPER for its unacknowledged page. The local cursor alone
    // cannot prove that a notification committed before a previous daemon died.
    for entry in handle.uncommitted_outcomes()? {
        let semantic = match entry.update {
            Update::Execution { run, update } => NativeActorEvent::Execution { run, update },
            Update::Attention(evidence) => NativeActorEvent::Attention(evidence),
            _ => return Err("The retained semantic outbox contains an unsupported event".into()),
        };
        let (changed, automation) = {
            let mut registry = registry.lock().unwrap_or_else(|p| p.into_inner());
            let changed = registry.commit_native_actor_event(identity.instance, &identity.record, &source, entry.sequence, &semantic, chat::now_ms().unwrap_or(0))?;
            let automation = registry.agent_for_session(id)?.is_some_and(|agent| matches!(agent.origin, crate::agent_execution::Origin::Automation { .. }));
            (changed, automation)
        };
        if changed { super::super::agent_execution::announce(host, &registry); }
        if let NativeActorEvent::Attention(evidence) = semantic {
            // Scheduled notifications are produced from committed run outcomes.
            if !automation {
                let receipt = notifications.as_ref().ok_or("The notification journal is unavailable; attention remains retained")?.record_committed(crate::notifications::Event::session(id, source.launch.provider, source.launch.recipe.working_directory.clone(), evidence))?;
                receipt.recv_timeout(Duration::from_secs(10)).map_err(|_| "The notification journal has not confirmed this attention outcome")??;
            }
        }
        let current = host.lock().unwrap_or_else(|p| p.into_inner()).chats.get(&id).is_some_and(|record| record.handle.same_actor(&handle) && record.session == source && record.execution_instance == identity.instance);
        if !current { return Err("Retained actor ownership changed before semantic acknowledgement".into()); }
        handle.commit_retained_outcomes(entry.sequence)?;
    }
    Ok(())
}

/// Keep the exact cleanup owner even when helper spawn/installation replies
/// fail. A later retry must prove this owner's teardown before any new launch.
pub(super) fn forget_startup(host: &SharedHost, source: &Session, context: &Arc<chat::protocol::ExecutionContext>) {
    let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
    if state.native_actor_startups.get(&source.id).is_some_and(|(expected, current)| expected == source && Arc::ptr_eq(current, context)) {
        state.native_actor_startups.remove(&source.id);
    }
}

pub(super) fn reconcile_startup(host: &SharedHost, source: &Session) -> Result<bool, String> {
    let pending = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        let Some((expected, context)) = state.native_actor_startups.get(&source.id) else { return Ok(false); };
        if state.chats.contains_key(&source.id) || state.chat_spawning.contains_key(&source.id)
            || expected.launch != source.launch || expected.terminal != source.terminal
            || expected.owned_codex_listener != source.owned_codex_listener
            || source.proof.as_ref().is_some_and(|proof| Some(proof) != expected.proof.as_ref())
        {
            return Err("The retained startup belongs to another exact descriptor or is still transitioning".into());
        }
        (expected.clone(), context.clone())
    };
    match pending.1.reconcile_cleanup()? {
        chat::protocol::LaunchOutcome::NoProvider | chat::protocol::LaunchOutcome::TeardownProved => {
            forget_startup(host, &pending.0, &pending.1);
            Ok(true)
        }
        chat::protocol::LaunchOutcome::Uncertain => Err("The exact retained startup has not proved cleanup; no second provider was started".into()),
    }
}

/// Reattach to the resident Claude reducer; never open provider stdin, reload
/// its transcript as a replacement, or reconstruct command/control UUIDs.
pub(super) fn restore(host: &SharedHost, client: ClientId, requested: &Session, proof: &HostSessionProof, owner: crate::daemon::holder_registry::Record) -> Result<ServerResponse, String> {
    let inspection = Handle::inspect_remote(&owner, proof.clone())?;
    if inspection.identity.record != owner || inspection.source.id != requested.id
        || inspection.source.launch.provider != Provider::Claude
    { return Err("The retained Claude actor identity changed during inspection".into()); }
    // Pair adoption is qualified independently against its retained worker;
    // until that metadata is available, preserve ownership rather than drop it.
    if requested.terminal.is_some() || inspection.source.terminal.is_some() {
        return Err("The retained Claude actor has a paired terminal whose exact worker must be reconciled before attachment".into());
    }
    let weak = Arc::downgrade(host);
    let id = requested.id;
    let handle = Handle::adopt_remote(&owner, requested.clone(), proof.clone(), move |update| changed(&weak, id, update))?;
    let identity = handle.actor_identity().ok_or("The retained actor did not provide its stable identity")?;
    if identity != inspection.identity { return Err("The retained actor changed while attaching".into()); }
    let mut session = requested.clone();
    session.proof = Some(proof.clone());
    session.launch.session_id = handle.confirmed_session_id().or(session.launch.session_id);
    handle.update_native_source(session.clone(), None, None)?;
    let mut sources = inspection.previous_sources;
    sources.push(inspection.source);
    sources.push(requested.clone());
    sources.sort_by_key(|source| serde_json::to_vec(source).unwrap_or_default());
    sources.dedup();
    let registry = super::super::agent_execution::registry(host)?;
    registry.lock().unwrap_or_else(|p| p.into_inner()).adopt_authenticated_agent_session(
        &sources.iter().cloned().map(crate::agent_execution::HostedSession::Chat).collect::<Vec<_>>(),
        &crate::agent_execution::HostedSession::Chat(session.clone()),
    )?;
    handle.commit_native_installation()?;
    session.native_continuity = handle.native_continuity();
    let observation = Observation { after_generation: handle.observe_next_update(), ..Observation::default() };
    let previous_sources = super::native_recovery::retained_upgrade_sources(sources, &session);
    let record = Record {
        execution_instance: identity.instance, console_recipe: None, paired_terminal: None,
        session, handle: handle.clone(), source_process: None,
        subscribers: if client == 0 { BTreeMap::new() } else { BTreeMap::from([(client, false)]) },
        observations: if client == 0 { BTreeMap::new() } else { BTreeMap::from([(client, observation)]) },
        completion_observed: false, returning_terminal: false,
    };
    let response = session_snapshot(&record);
    let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
    if state.shutting_down || state.proof() != proof {
        return Err("Daemon ownership changed before retained Claude registration; the provider remains owned by its holder".into());
    }
    let retired = state.chats.insert(id, record);
    state.upgrade_chats.insert(id, previous_sources);
    state.native_adoption_failures.clear(id);
    state.inventory_generation += 1;
    drop(state);
    drop(retired);
    handle.wake_retained_outcomes();
    super::super::state::retire_unattached_upgrade_chat(host, id);
    Ok(response)
}
