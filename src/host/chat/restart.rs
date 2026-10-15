//! A restart owns the whole actor/pair publication, including retry after I/O failure.
use super::*;
use super::super::agent_execution::{PairPlan, prepare_restart_pair, restore_restart_pair};

const MAX_ALIASES: usize = 32;
const MAX_ALIAS_BYTES:usize=1024*1024;
fn aliases_fit(aliases:&[Session])->bool {
    aliases.len()<=MAX_ALIASES && aliases.iter().try_fold(0usize,|bytes,alias|{
        let size=serde_json::to_vec(alias).ok()?.len();
        bytes.checked_add(size).filter(|bytes|*bytes<=MAX_ALIAS_BYTES)
    }).is_some()
}

pub(in crate::host) struct Restart {
    plan: PairPlan,
    aliases: Vec<Session>,
    busy: bool,
    actor: Option<Handle>,
    receipt: Option<chat::RestartReceipt>,
}
impl Restart {
    pub(in crate::host) fn pending(&self) -> bool { self.busy || self.receipt.is_none() }
}

struct Claim { host: SharedHost, id: TerminalId }
impl Drop for Claim {
    fn drop(&mut self) {
        if let Some(restart) = self.host.lock().unwrap_or_else(|p|p.into_inner()).chat_restarts.get_mut(&self.id) {
            restart.busy = false;
        }
    }
}

pub(super) fn run(host:&SharedHost, client:ClientId, source:&Session, execution:Option<Arc<chat::protocol::ExecutionContext>>) -> Result<Option<ServerResponse>,ProtocolError> {
    let (plan, actor, aliases) = {
        let mut state = host.lock().unwrap_or_else(|p|p.into_inner());
        validate_proof(source.proof.as_ref(),state.proof())?;
        if let Some(restart) = state.chat_restarts.get(&source.id) {
            if restart.busy { return Err(unavailable("This conversation is already restarting; retry its exact pane after the current attempt")); }
            if !restart.aliases.contains(source) && !restart.receipt.as_ref().is_some_and(|receipt|receipt.replacement()==source) { return Err(error("The saved pane is not an issued alias of this exact restart")); }
            let actor = restart.actor.as_ref().filter(|actor| state.chats.get(&source.id).is_some_and(|record| record.handle.same_actor(actor) && !actor.is_completed())).cloned();
            if let Some(actor) = actor {
                let plan = restart.plan.clone();
                let receipt = restart.receipt.clone();
                state.chat_restarts.get_mut(&source.id).expect("observed restart").busy=true;
                (Some((plan,receipt)),Some(actor),Vec::new())
            } else {
                let aliases = restart.aliases.clone();
                if aliases.len() >= MAX_ALIASES { return Err(unavailable("This pane has reached the bounded restart history. Save and reopen its current exact descriptor before restarting again")); }
                (None,None,aliases)
            }
        } else { (None,None,Vec::new()) }
    };
    if let Some((plan,receipt))=plan {
        let _claim=Claim{host:host.clone(),id:source.id};
        let receipt=match receipt { Some(receipt)=>receipt, None=>restore_restart_pair(host,client,&plan).map_err(unavailable)? };
        return publish(host,client,source,actor.as_ref().expect("observed actor"),receipt).map(Some);
    }
    let (canonical,old_actor) = {
        let state=host.lock().unwrap_or_else(|p|p.into_inner());
        let Some(record)=state.chats.get(&source.id) else {return Ok(None)};
        if !record.handle.is_completed() { return Ok(None); }
        let ServerResponse::ChatAttached{session,..}=session_snapshot(record) else {unreachable!()};
        // A known alias was previously authenticated. A first request must
        // match the full immutable source and any pair it actually supplies.
        if aliases.is_empty() && (source.id!=session.id || source.proof!=session.proof
            || source.launch.provider!=session.launch.provider || source.launch.recipe!=session.launch.recipe
            || source.launch.session_id.as_ref().is_some_and(|id| session.launch.session_id.as_ref()!=Some(id))
            || source.owned_codex_listener!=session.owned_codex_listener
            || source.terminal.as_ref().is_some_and(|terminal| session.terminal.as_ref()!=Some(terminal))) {
            return Err(error("The requested restart does not match the exact hosted conversation and pair"));
        }
        (session,record.handle.clone())
    };
    let plan=prepare_restart_pair(host,&canonical).map_err(unavailable)?;
    {
        let mut state=host.lock().unwrap_or_else(|p|p.into_inner());
        if state.chat_restarts.get(&source.id).is_some_and(|restart|restart.busy)
            || state.chats.get(&source.id).is_none_or(|record|!record.handle.same_actor(&old_actor)||!record.handle.is_completed()) {
            return Err(unavailable("The conversation owner changed before restart; no second writer was started"));
        }
        let ServerResponse::ChatAttached{session:observed,..}=session_snapshot(&state.chats[&source.id]) else{unreachable!()};
        if observed!=*plan.source() {return Err(unavailable("The exact provider descriptor changed during restart admission; no owner was stopped"));}
        let mut aliases=aliases;
        if !aliases.contains(source) { aliases.push(source.clone()); }
        if !aliases.contains(&canonical) { aliases.push(canonical); }
        if !aliases_fit(&aliases) {return Err(unavailable("The bounded exact restart history is full; save the current pane before restarting"));}
        state.chat_restarts.insert(source.id,Restart{plan:plan.clone(),aliases,busy:true,actor:None,receipt:None});
    }
    let _claim=Claim{host:host.clone(),id:source.id};
    super::restart_completed_inner(host,client,plan.source(),execution)?
        .ok_or_else(||unavailable("The exact stopped actor changed before restart"))?;
    let actor={
        let mut state=host.lock().unwrap_or_else(|p|p.into_inner());
        let actor=state.chats.get(&source.id).map(|record|record.handle.clone()).ok_or_else(||unavailable("The restarted actor disappeared before pair recovery"))?;
        state.chat_restarts.get_mut(&source.id).ok_or_else(||unavailable("The restart was closed before its actor receipt could be retained"))?.actor=Some(actor.clone());
        actor
    };
    let deadline=Instant::now()+Duration::from_secs(30);
    while actor.confirmed_session_id().is_none() || !actor.update_activity().1 {
        if actor.is_completed() || Instant::now()>=deadline {
            return Err(unavailable("The replacement provider has not confirmed readiness; its exact actor and saved pane were retained. Retry this pane to finish console recovery"));
        }
        thread::sleep(Duration::from_millis(20));
    }
    let receipt=restore_restart_pair(host,client,&plan).map_err(unavailable)?;
    publish(host,client,source,&actor,receipt).map(Some)
}

fn publish(host:&SharedHost,client:ClientId,source:&Session,actor:&Handle,receipt:chat::RestartReceipt)->Result<ServerResponse,ProtocolError>{
    let receipt=chat::RestartReceipt::confirm(source.clone(),receipt.replacement().clone()).map_err(unavailable)?;
    let mut state=host.lock().unwrap_or_else(|p|p.into_inner());
    let record=state.chats.get_mut(&source.id).filter(|record|record.handle.same_actor(actor)).ok_or_else(||unavailable("The restarted actor changed before its receipt was delivered"))?;
    record.subscribers.entry(client).or_insert(false);
    record.observations.entry(client).or_insert_with(||Observation{after_generation:record.handle.observe_next_update(),..Default::default()});
    let snapshot=record.handle.snapshot(None).expect("unconditional snapshot");
    if snapshot.session_id!=receipt.replacement().launch.session_id {return Err(unavailable("The restarted actor has not confirmed its exact conversation identity"));}
    let restart=state.chat_restarts.get_mut(&source.id).ok_or_else(||unavailable("The restart was closed before its receipt was published"))?;
    restart.receipt=Some(receipt.clone());
    Ok(ServerResponse::ChatRestarted{receipt,snapshot})
}

pub(super) fn require_not_busy(host:&SharedHost,id:TerminalId)->Result<(),ProtocolError>{
    if host.lock().unwrap_or_else(|p|p.into_inner()).chat_restarts.get(&id).is_some_and(|restart|restart.busy) {
        return Err(unavailable("The exact provider restart is still publishing its console; retry after it finishes"));
    }
    Ok(())
}
pub(super) fn require_finished(host:&SharedHost,id:TerminalId)->Result<(),ProtocolError>{
    if host.lock().unwrap_or_else(|p|p.into_inner()).chat_restarts.get(&id).is_some_and(Restart::pending) {
        return Err(unavailable("Finish the retained provider restart before returning its console to terminal"));
    }
    Ok(())
}

pub(super) fn transition_gate(host:&SharedHost,id:TerminalId)->Arc<std::sync::Mutex<()>> {
    let mut state=host.lock().unwrap_or_else(|p|p.into_inner());
    state.chat_transition_gates.retain(|_,gate|gate.strong_count()>0);
    if let Some(gate)=state.chat_transition_gates.get(&id).and_then(std::sync::Weak::upgrade){return gate;}
    let gate=Arc::new(std::sync::Mutex::new(()));
    state.chat_transition_gates.insert(id,Arc::downgrade(&gate));
    gate
}
