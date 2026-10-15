//! One coalescing semantic-outcome broker for all helper-owned actors. Provider
//! callbacks only wake it; neither callback return nor enqueueing acknowledges
//! a durable outcome. No RPC, SQLite transaction or journal wait holds HostState.
use super::*;
mod restart;
pub(super) use self::restart::{restart_completed, restart_failure_proved};
pub(in crate::host) use self::restart::PreparedRestart;
use crate::host::model::NativeActorEvent;
use std::{collections::BTreeSet, sync::{Condvar, Mutex}};

#[derive(Clone)]
pub(in crate::host) struct Startup {
    pub(super) source: Session,
    pub(super) requested: Session,
    pub(super) context: Arc<chat::protocol::ExecutionContext>,
}

/// Unit-only HostState fixtures intentionally exercise local transports without
/// a daemon lease. The retained-actor integration fixture opts into the genuine
/// exec/socket/lease boundary. Production has no fallback or environment knob.
pub(super) fn enabled(provider: Provider, transport: &chat::protocol::TransportSpec) -> bool {
    #[cfg(test)]
    let enabled = std::env::var_os("CAGENTS_RETAINED_ACTOR_FIXTURE").is_some();
    #[cfg(not(test))]
    let enabled = true;
    enabled && provider == Provider::Claude && matches!(transport, chat::protocol::TransportSpec::JsonLines)
}

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
                Ok(more) => {
                    warned.remove(&id);
                    if more { retry.insert(id); }
                }
                Err(error) => {
                    if warned.insert(id) { tracing::warn!(%id, %error, "retained actor outcome is not committed; helper retains it for retry"); }
                    retry.insert(id);
                }
            }
        }
    }
}

fn drain(host: &SharedHost, id: TerminalId) -> Result<bool, String> {
    let (handle, source, identity, notifications) = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        let Some(record) = state.chats.get(&id) else { return Ok(false); };
        let Some(identity) = record.handle.actor_identity() else { return Ok(false); };
        (record.handle.clone(), record.session.clone(), identity, state.notifications.clone())
    };
    let registry = super::super::agent_execution::registry(host)?;
    handle.refresh_retained_state()?;
    let loss = handle.continuity_loss();
    if let Some(loss) = &loss {
        let changed = registry.lock().unwrap_or_else(|p| p.into_inner()).commit_native_actor_loss(
            identity.instance, &identity.record, &source, &loss.affected_runs, &loss.reason, chat::now_ms().unwrap_or(0))?;
        if changed { super::super::agent_execution::announce(host, &registry); }
        let current = host.lock().unwrap_or_else(|p| p.into_inner()).chats.get(&id).is_some_and(|record|
            record.handle.same_actor(&handle) && record.execution_instance == identity.instance && record.session == source);
        if !current { return Err("Retained actor changed before continuity-loss acknowledgement".into()); }
        handle.ack_continuity_loss(loss.id)?;
    }
    // Always ask the HELPER for its unacknowledged page. The local cursor alone
    // cannot prove that a notification committed before a previous daemon died.
    let page = handle.uncommitted_outcomes()?;
    let more = !page.is_empty();
    for entry in page {
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
            if !automation && !(loss.is_some() && evidence.kind == crate::notifications::Kind::Succeeded) {
                let receipt = notifications.as_ref().ok_or("The notification journal is unavailable; attention remains retained")?.record_committed(crate::notifications::Event::session(id, source.launch.provider, source.launch.recipe.working_directory.clone(), evidence))?;
                receipt.recv_timeout(Duration::from_secs(10)).map_err(|_| "The notification journal has not confirmed this attention outcome")??;
            }
        }
        let current = host.lock().unwrap_or_else(|p| p.into_inner()).chats.get(&id).is_some_and(|record| record.handle.same_actor(&handle) && record.session == source && record.execution_instance == identity.instance);
        if !current { return Err("Retained actor ownership changed before semantic acknowledgement".into()); }
        handle.commit_retained_outcomes(entry.sequence)?;
    }
    // One bounded page per actor per pass. A detached actor can have many
    // pages without another latest-sequence edge; explicitly revisit it until
    // an empty page confirms the helper's entire outbox is acknowledged.
    Ok(more)
}

/// Keep the exact cleanup owner even when helper spawn/installation replies
/// fail. A later retry must prove this owner's teardown before any new launch.
pub(super) fn forget_startup(host: &SharedHost, source: &Session, context: &Arc<chat::protocol::ExecutionContext>) {
    let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
    if state.native_actor_startups.get(&source.id).is_some_and(|pending| &pending.source == source && Arc::ptr_eq(&pending.context, context)) {
        state.native_actor_startups.remove(&source.id);
    }
}

pub(super) fn reconcile_startup(host: &SharedHost, source: &Session) -> Result<bool, String> {
    let pending = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        let Some(pending) = state.native_actor_startups.get(&source.id) else { return Ok(false); };
        if state.chats.contains_key(&source.id) || state.chat_spawning.contains_key(&source.id)
            || (&pending.source != source && &pending.requested != source)
        {
            return Err("The retained startup belongs to another exact descriptor or is still transitioning".into());
        }
        (pending.source.clone(), pending.context.clone())
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
    let weak = Arc::downgrade(host);
    let id = requested.id;
    let expected_instance = inspection.identity.instance;
    let handle = Handle::adopt_remote(&owner, requested.clone(), proof.clone(), move |update| changed_owned(&weak, id, expected_instance, update))?;
    let identity = handle.actor_identity().ok_or("The retained actor did not provide its stable identity")?;
    if identity != inspection.identity { return Err("The retained actor changed while attaching".into()); }
    let mut session = requested.clone();
    session.proof = Some(proof.clone());
    session.launch.session_id = handle.confirmed_session_id().or(session.launch.session_id);
    // A presentation alias predating the console cannot release its current
    // exact paired worker. Only authenticated helper metadata names that pair.
    session.terminal = inspection.source.terminal.clone();
    let paired = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        match (&mut session.terminal, inspection.paired_holder.as_ref()) {
            (None, None) => None,
            (Some(terminal), Some(owner)) => {
                let current = state.terminals.get(&terminal.id).filter(|current|
                    current.handle.holder_identity() == Some(owner)
                    && current.session.launch == terminal.launch
                    && current.session.owned_codex_listener == terminal.owned_codex_listener)
                    .ok_or("The retained Claude console's exact worker could not be recovered; provider ownership was preserved")?;
                *terminal = state.native_terminal_projection(terminal.id)?;
                if terminal.continuity.live_proof().as_ref() != Some(proof) {
                    return Err("The retained Claude console belongs to a different daemon owner".into());
                }
                Some(current.handle.clone())
            }
            _ => return Err("The retained Claude pair lacks exact worker evidence; no pair was released".into()),
        }
    };
    handle.update_native_source(session.clone(), inspection.execution_console.clone(), inspection.paired_holder.clone())?;
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
        execution_instance: identity.instance, console_recipe: inspection.execution_console, paired_terminal: paired.clone(),
        session, handle: handle.clone(), source_process: None,
        subscribers: if client == 0 { BTreeMap::new() } else { BTreeMap::from([(client, false)]) },
        observations: if client == 0 { BTreeMap::new() } else { BTreeMap::from([(client, observation)]) },
        completion_observed: false, returning_terminal: false,
    };
    let response = session_snapshot(&record);
    let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
    let pair_unchanged = match (&record.session.terminal, &paired) {
        (None, None) => true,
        (Some(terminal), Some(handle)) => state.terminals.get(&terminal.id).is_some_and(|current|
            current.handle.same_worker(handle)
                && state.native_terminal_projection(terminal.id).as_ref() == Ok(terminal)),
        _ => false,
    };
    if state.shutting_down || state.proof() != proof || !pair_unchanged {
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

/// These IDs came from the authenticated actor's journal at attachment, not
/// from a user pane, busy bit, or a prior daemon's accepted dispatch record.
pub(in crate::host) fn retained_runs(host: &SharedHost) -> Vec<(crate::model::RunId, crate::agent_execution::HostedSession)> {
    let actors = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        state.chats.values().filter_map(|record| {
            let identity = record.handle.actor_identity()?;
            (identity.instance == record.execution_instance).then(|| (record.handle.clone(), record.session.clone()))
        }).collect::<Vec<_>>()
    };
    actors.into_iter().flat_map(|(handle, source)| handle.retained_runs().into_iter()
        .map(move |run| (run, crate::agent_execution::HostedSession::Chat(source.clone())))).collect()
}

/// Boot must record a known loss before deciding which accepted runs remain
/// protected by the surviving actor. Ordinary deferred delivery still retries.
pub(in crate::host) fn reconcile_startup_losses(host: &SharedHost) -> Result<(), String> {
    let actors = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        state.chats.values().filter_map(|record| record.handle.actor_identity().map(|identity|
            (record.handle.clone(), record.session.clone(), identity))).collect::<Vec<_>>()
    };
    let registry = super::super::agent_execution::registry(host)?;
    for (handle, source, identity) in actors {
        handle.refresh_retained_state()?;
        if let Some(loss) = handle.continuity_loss() {
            let changed = registry.lock().unwrap_or_else(|p| p.into_inner()).commit_native_actor_loss(
                identity.instance, &identity.record, &source, &loss.affected_runs, &loss.reason, chat::now_ms().unwrap_or(0))?;
            if changed { super::super::agent_execution::announce(host, &registry); }
            handle.ack_continuity_loss(loss.id)?;
        }
    }
    Ok(())
}

/// Provider Close and helper retirement are separate. Keep its reducer/Store
/// and final semantic journal until every required daemon write is committed.
pub(in crate::host) fn finish_close(host: &SharedHost, handle: &Handle, requested: &Session) -> Result<(), String> {
    let Some(identity) = handle.actor_identity() else { return Ok(()); };
    if !identity.record.holder.is_current() {
        // A failed terminal spawn may retry after the previous close already
        // committed every outcome and retired the helper. Only its separate
        // exact semantic-retirement receipt can authorize this fast path.
        return handle.finish_retained_close();
    }
    let (proof, notifications) = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        (state.proof().clone(), state.notifications.clone())
    };
    let registry = super::super::agent_execution::registry(host)?;
    let source = {
        let registry = registry.lock().unwrap_or_else(|p| p.into_inner());
        match registry.agent_for_session(requested.id)?.and_then(|agent| agent.session) {
            Some(crate::agent_execution::HostedSession::Chat(source)) => source,
            Some(_) => return Err("The closing native actor changed execution surface".into()),
            None => requested.clone(),
        }
    };
    if !chat::native_holder::closed_source(&source, &proof)?.is_some_and(|(owner, _)| owner == identity.record) {
        return Err("The final actor publication does not prove this exact execution descriptor closed".into());
    }
    handle.refresh_retained_state()?;
    let loss = handle.continuity_loss();
    if let Some(loss) = &loss {
        let changed = registry.lock().unwrap_or_else(|p| p.into_inner()).commit_native_actor_loss(identity.instance, &identity.record, &source, &loss.affected_runs, &loss.reason, chat::now_ms().unwrap_or(0))?;
        if changed { super::super::agent_execution::announce(host, &registry); }
        let current = {
            let state = host.lock().unwrap_or_else(|p| p.into_inner());
            state.proof() == &proof && (state.chats.get(&source.id).is_some_and(|record| record.handle.same_actor(handle))
                || state.native_closing.get(&source.id).is_some_and(|pending| pending.owns(&identity.record, requested)))
        };
        if !current { return Err("Actor close ownership changed before continuity-loss acknowledgement".into()); }
        handle.ack_continuity_loss(loss.id)?;
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let page = handle.uncommitted_outcomes()?;
        if page.is_empty() { break; }
        for entry in page {
            let event = match entry.update {
                Update::Execution { run, update } => NativeActorEvent::Execution { run, update },
                Update::Attention(evidence) => NativeActorEvent::Attention(evidence),
                _ => return Err("Closing actor journal contains an unsupported semantic event".into()),
            };
            let (changed, automation) = {
                let mut registry = registry.lock().unwrap_or_else(|p| p.into_inner());
                let changed = registry.commit_native_actor_event(identity.instance, &identity.record, &source, entry.sequence, &event, chat::now_ms().unwrap_or(0))?;
                let automation = registry.agent_for_session(source.id)?.is_some_and(|agent| matches!(agent.origin, crate::agent_execution::Origin::Automation { .. }));
                (changed, automation)
            };
            if changed { super::super::agent_execution::announce(host, &registry); }
            if let NativeActorEvent::Attention(evidence) = event
                && !automation && !(loss.is_some() && evidence.kind == crate::notifications::Kind::Succeeded)
            {
                notifications.as_ref().ok_or("Final attention journal is unavailable; helper is retained")?
                    .record_committed(crate::notifications::Event::session(source.id, source.launch.provider, source.launch.recipe.working_directory.clone(), evidence))?
                    .recv_timeout(Duration::from_secs(10)).map_err(|_| "Final attention has not committed; helper is retained")??;
            }
            let current = {
                let state = host.lock().unwrap_or_else(|p| p.into_inner());
                state.proof() == &proof && (state.chats.get(&source.id).is_some_and(|record| record.handle.same_actor(handle))
                    || state.native_closing.get(&source.id).is_some_and(|pending| pending.owns(&identity.record, requested)))
            };
            if !current { return Err("Actor close ownership changed before final acknowledgement".into()); }
            handle.commit_retained_outcomes(entry.sequence)?;
            if Instant::now() >= deadline { return Err("Final actor outcomes remain pending; retry its retained close".into()); }
        }
    }
    handle.finish_retained_close()
}
