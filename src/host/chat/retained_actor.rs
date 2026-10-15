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
