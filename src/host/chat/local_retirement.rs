//! Custody of the exact original Local close while durable retirement retries.
//! Closing/absence is never positive proof: only the owning close call may seal.
use super::*;
use std::sync::atomic::{AtomicBool, Ordering};

pub(in crate::host) struct Pending {
    source: Session,
    actor: Handle,
    instance: uuid::Uuid,
    pair_at_capture: Option<crate::terminal::TerminalHandle>,
    closed: AtomicBool,
    committed: AtomicBool,
    final_state: std::sync::Mutex<Final>,
}
#[derive(Default)]
struct Final {
    snapshot: Option<chat::Snapshot>,
    pair: Option<(crate::model::TerminalSession, crate::terminal::TerminalHandle)>,
    chat_close: bool,
}

pub(in crate::host) fn require_open(state: &HostState, id: TerminalId) -> Result<(), String> {
    if state.local_chat_retirements.contains_key(&id) {
        return Err("This exact closed owner still has a pending durable retirement; retry its original close before starting or pairing another owner".into());
    }
    Ok(())
}
fn matches(state: &HostState, pending: &Pending) -> bool {
    state.chats.get(&pending.source.id).is_some_and(|record| {
        record.execution_instance == pending.instance && record.handle.same_actor(&pending.actor)
            && record.session == pending.source
            && match (&record.paired_terminal, &pending.pair_at_capture) {
                (None, None) => true,
                (Some(actual), Some(expected)) => actual.same_worker(expected),
                _ => false,
            }
    })
}
/// Reserve BEFORE close effects. An uncertain close retains the original
/// owner; it must not be confused with a sealed successful teardown.
pub(super) fn begin(host: &SharedHost, id: TerminalId, actor: &Handle) -> Result<Arc<Pending>, ProtocolError> {
    let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(pending) = state.local_chat_retirements.get(&id) {
        if !pending.actor.same_actor(actor) || !matches(&state, pending) {
            return Err(unavailable("The pending closed owner differs from the current actor; neither owner was discarded"));
        }
        return Ok(pending.clone());
    }
    let record = state.chats.get(&id).filter(|record| record.handle.same_actor(actor))
        .ok_or_else(|| unavailable("The exact actor changed before close custody was reserved"))?;
    let pending = Arc::new(Pending {source:record.session.clone(),actor:actor.clone(),
        instance:record.execution_instance,pair_at_capture:record.paired_terminal.clone(),closed:AtomicBool::new(false),committed:AtomicBool::new(false),
        final_state:std::sync::Mutex::new(Final::default())});
    // Each entry retains an existing Record; no arbitrary IDs/eviction and no
    // capacity escape independent of actual hosted inventory are admitted.
    state.local_chat_retirements.insert(id,pending.clone());
    Ok(pending)
}
impl Pending {
    /// Negative inventory only: immutable issued source, not current-path or
    /// foreign-project proof. Join actual operational scope or refuse unknown.
    pub(in crate::host) fn source(&self) -> &Session { &self.source }
    pub(super) fn sealed(&self) -> bool { self.closed.load(Ordering::Acquire) }
    pub(super) fn chat_sealed(&self) -> bool {
        self.sealed() && self.final_state.lock().unwrap_or_else(|p|p.into_inner()).chat_close
    }
    pub(super) fn snapshot(&self) -> Option<chat::Snapshot> {
        self.final_state.lock().unwrap_or_else(|p| p.into_inner()).snapshot.clone()
    }
    /// Only after the ACTUAL actor close and, for ordinary chat close, exact
    /// paired shutdown succeeded. A completed flag or cache read cannot seal.
    pub(super) fn seal(&self, snapshot:Option<chat::Snapshot>,
        pair:Option<(crate::model::TerminalSession,crate::terminal::TerminalHandle)>, chat_close:bool) {
        let mut final_state=self.final_state.lock().unwrap_or_else(|p|p.into_inner());
        if !self.closed.load(Ordering::Acquire) || (chat_close && !final_state.chat_close) {
            *final_state=Final {snapshot,pair,chat_close};
            self.closed.store(true,Ordering::Release);
        }
    }
}
/// Native cleanup has its own stronger custody/commit sequence. Release this
/// reservation only after that owning path returned its positive disposition.
pub(super) fn finished_elsewhere(host:&SharedHost,pending:&Arc<Pending>) {
    let removed={let mut state=host.lock().unwrap_or_else(|p|p.into_inner());
        if state.local_chat_retirements.get(&pending.source.id).is_some_and(|current|Arc::ptr_eq(current,pending)) {
            state.local_chat_retirements.remove(&pending.source.id)
        } else {None}};
    drop(removed);
}

pub(super) fn commit(host:&SharedHost,pending:&Arc<Pending>)->Result<(),ProtocolError> {
    if !pending.sealed() {return Err(unavailable("The original close has not proved teardown; its owner remains retained"));}
    let final_state=pending.final_state.lock().unwrap_or_else(|p|p.into_inner());
    {
        let state=host.lock().unwrap_or_else(|p|p.into_inner());
        if !matches(&state,pending) {return Err(unavailable("The exact closed actor changed before durable retirement; its replacement was preserved"));}
        if let Some((source,worker))=&final_state.pair
            && state.terminals.get(&source.id).is_none_or(|record|
                !matches_paired_terminal(&record.session,source,state.proof()) || !record.handle.same_worker(worker)) {
            return Err(unavailable("The paired worker changed before durable retirement; its replacement was preserved"));
        }
    }
    if !pending.committed.load(Ordering::Acquire) {
        // SQL and model notification are outside HostState. A failure leaves
        // Record, actual Handle, final snapshot and exact pair in custody.
        super::super::agent_execution::retired(host,&pending.source).map_err(unavailable)?;
        pending.committed.store(true,Ordering::Release);
    }
    let (record,pair,removed_pending)={
        let mut state=host.lock().unwrap_or_else(|p|p.into_inner());
        if !matches(&state,pending) {return Err(unavailable("The actor changed after retirement committed; its replacement was preserved"));}
        if let Some((source,worker))=&final_state.pair
            && state.terminals.get(&source.id).is_none_or(|record|
                !matches_paired_terminal(&record.session,source,state.proof()) || !record.handle.same_worker(worker)) {
            return Err(unavailable("The paired worker changed after retirement committed; its replacement was preserved"));
        }
        state.issued_chat_ids.insert(pending.source.id);
        state.inventory_generation+=1;
        let record=state.chats.remove(&pending.source.id);
        if final_state.chat_close {
            state.chat_restarts.remove(&pending.source.id);
            if let Some(notifications)=&state.notifications {notifications.retire(pending.source.id);}
        }
        let pair=final_state.pair.as_ref().and_then(|(source,_)|state.retire_terminal(source.id));
        let removed_pending=state.local_chat_retirements.remove(&pending.source.id);
        (record,pair,removed_pending)
    };
    drop(final_state);
    // Last owner/worker destruction must remain outside HostState.
    drop((record,pair,removed_pending));
    Ok(())
}
