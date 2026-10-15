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
    request: uuid::Uuid,
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
            if restart.receipt.is_none() && restart.actor.is_none() {
                let plan=restart.plan.clone();
                state.chat_restarts.get_mut(&source.id).expect("observed restart").busy=true;
                (Some((plan,None)),None,Vec::new())
            } else {
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
            }
        } else { (None,None,Vec::new()) }
    };
    if let Some((plan,receipt))=plan {
        let _claim=Claim{host:host.clone(),id:source.id};
        let Some(actor)=actor else {return finish_backend(host,client,source,execution,&plan).map(Some);};
        if receipt.is_none() { wait_ready(&actor)?; }
        let receipt=match receipt { Some(receipt)=>receipt, None=>restore_restart_pair(host,client,&plan).map_err(unavailable)? };
        return publish(host,client,source,&actor,receipt).map(Some);
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
        state.chat_restarts.insert(source.id,Restart{plan:plan.clone(),aliases,busy:true,actor:None,receipt:None,request:uuid::Uuid::new_v4()});
    }
    let _claim=Claim{host:host.clone(),id:source.id};
    finish_backend(host,client,source,execution,&plan).map(Some)
}

fn finish_backend(host:&SharedHost,client:ClientId,source:&Session,execution:Option<Arc<chat::protocol::ExecutionContext>>,plan:&PairPlan)->Result<ServerResponse,ProtocolError>{
    let _request_id=request_id(host,source.id)?;
    super::restart_completed_inner(host,client,plan.source(),execution)?
        .ok_or_else(||unavailable("The exact stopped actor changed before restart"))?;
    let actor={
        let mut state=host.lock().unwrap_or_else(|p|p.into_inner());
        let actor=state.chats.get(&source.id).map(|record|record.handle.clone()).ok_or_else(||unavailable("The restarted actor disappeared before pair recovery"))?;
        state.chat_restarts.get_mut(&source.id).ok_or_else(||unavailable("The restart was closed before its actor receipt could be retained"))?.actor=Some(actor.clone());
        actor
    };
    wait_ready(&actor)?;
    let receipt=restore_restart_pair(host,client,&plan).map_err(unavailable)?;
    publish(host,client,source,&actor,receipt)
}

fn wait_ready(actor:&Handle)->Result<(),ProtocolError>{
    let deadline=Instant::now()+Duration::from_secs(30);
    while actor.confirmed_session_id().is_none() || !actor.update_activity().1 {
        if actor.is_completed() || Instant::now()>=deadline {
            return Err(unavailable("The replacement provider has not confirmed readiness; its exact actor and saved pane were retained. Retry this pane to finish console recovery"));
        }
        thread::sleep(Duration::from_millis(20));
    }
    Ok(())
}

pub(super) fn request_id(host:&SharedHost,id:TerminalId)->Result<uuid::Uuid,ProtocolError>{
    host.lock().unwrap_or_else(|p|p.into_inner()).chat_restarts.get(&id).filter(|restart|restart.busy).map(|restart|restart.request)
        .ok_or_else(||unavailable("The provider restart has no active exact request claim"))
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn transition_claim_is_exclusive_and_weak_entries_are_pruned() {
        let host=Arc::new(HostLock::new(HostState::new(HostSessionProof{host_session_id:"claim".into(),boot_id:"boot".into(),host_incarnation:"same-daemon".into()})));
        let id=TerminalId::new();let first=transition_gate(&host,id);let held=first.lock().unwrap();
        let second=transition_gate(&host,id);assert!(second.try_lock().is_err());
        drop(held);assert!(second.try_lock().is_ok());drop(first);drop(second);
        drop(transition_gate(&host,TerminalId::new()));
        assert_eq!(host.lock().unwrap().chat_transition_gates.len(),1);
    }
    #[test]
    fn restart_alias_budget_bounds_both_count_and_descriptor_bytes() {
        let session=chat::fresh_session(crate::agent::Provider::Omp,TerminalId::new(),std::env::temp_dir(),&crate::account::Account::system(crate::agent::Provider::Omp)).unwrap();
        assert!(aliases_fit(&[session.clone()]));
        assert!(!aliases_fit(&vec![session.clone();MAX_ALIASES+1]));
        let mut large=session;large.launch.recipe.environment.insert("FIXTURE".into(),"x".repeat(MAX_ALIAS_BYTES));
        assert!(!aliases_fit(&[large]));
    }
}

#[cfg(test)]
mod process_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    #[test]
    fn repeated_restart_and_lost_reply_keep_one_exact_provider_actor() {
        const CHILD:&str="CAGENTS_EXACT_RESTART_FIXTURE";
        if std::env::var_os(CHILD).is_none(){
            let directory=tempfile::tempdir().unwrap();let root=directory.path();std::fs::create_dir(root.join("run")).unwrap();
            assert!(std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact","host::chat::restart::process_tests::repeated_restart_and_lost_reply_keep_one_exact_provider_actor","--nocapture"])
                .env_clear().env(CHILD,root).env("HOME",root).env("XDG_RUNTIME_DIR",root.join("run"))
                .env("XDG_DATA_HOME",root.join("data")).env("XDG_CONFIG_HOME",root.join("config")).env("XDG_CACHE_HOME",root.join("cache"))
                .env("PATH","/usr/bin:/bin").status().unwrap().success());return;
        }
        let root=std::path::PathBuf::from(std::env::var_os(CHILD).unwrap());let executable=root.join("omp");
        std::fs::write(&executable,r#"#!/usr/bin/python3
import json,os,sys
root=os.environ['CAGENTS_EXACT_RESTART_FIXTURE']
with open(root+'/spawns','a') as f:f.write(str(os.getpid())+'\n')
identity=sys.argv[sys.argv.index('--resume')+1]
print(json.dumps({'type':'ready'}),flush=True)
for line in sys.stdin:
    if os.path.exists(root+'/crash'):os._exit(17)
    request=json.loads(line)
    data={'sessionId':identity,'messages':[],'isStreaming':False,'isCompacting':False}
    print(json.dumps({'type':'response','id':request['id'],'command':request['type'],'success':True,'data':data}),flush=True)
"#).unwrap();std::fs::set_permissions(&executable,std::fs::Permissions::from_mode(0o700)).unwrap();
        let proof=HostSessionProof{host_session_id:"exact-restart".into(),boot_id:crate::daemon::read_boot_id().unwrap(),host_incarnation:"same-daemon".into()};
        let host=Arc::new(HostLock::new(HostState::new(proof.clone())));host.lock().unwrap().register_client(1);
        let mut source=Session{id:TerminalId::new(),launch:chat::Launch{provider:crate::agent::Provider::Omp,recipe:crate::model::LaunchRecipe{program:executable.to_string_lossy().into_owned(),arguments:vec![],working_directory:root.clone(),environment:Default::default(),unset_environment:Default::default()},session_id:Some("11111111-1111-4111-8111-111111111111".into())},proof:Some(proof),terminal:None,owned_codex_listener:None,native_continuity:Default::default()};
        super::super::start(&host,1,source.clone(),None,false,None,Default::default()).unwrap();
        let mut actor=host.lock().unwrap().chats[&source.id].handle.clone();wait_ready(&actor).unwrap();
        for generation in 0..2 {
            std::fs::write(root.join("crash"),b"1").unwrap();let _=actor.command(chat::Command::Refresh);
            let deadline=Instant::now()+Duration::from_secs(5);
            while !actor.is_completed(){assert!(Instant::now()<deadline);thread::sleep(Duration::from_millis(10));}
            std::fs::remove_file(root.join("crash")).unwrap();
            let gate=transition_gate(&host,source.id);let held=gate.lock().unwrap();
            assert!(super::super::process(&host,1,ClientRequest::ChatAttach{session:source.clone()}).is_err());drop(held);
            let ServerResponse::ChatRestarted{receipt,..}=super::super::process(&host,1,ClientRequest::ChatAttach{session:source.clone()}).unwrap() else {panic!("missing typed receipt")};
            assert_eq!(receipt.source(),&source);assert_eq!(receipt.replacement().launch,source.launch);
            let replacement=host.lock().unwrap().chats[&source.id].handle.clone();assert!(!replacement.same_actor(&actor));
            // Lost acknowledgment retries the same private actor, not a new process.
            assert!(matches!(super::super::process(&host,1,ClientRequest::ChatAttach{session:source.clone()}).unwrap(),ServerResponse::ChatRestarted{..}));
            assert!(host.lock().unwrap().chats[&source.id].handle.same_actor(&replacement));
            assert_eq!(std::fs::read_to_string(root.join("spawns")).unwrap().lines().count(),generation+2);
            source=receipt.replacement().clone();actor=replacement;
        }
        super::super::process(&host,1,ClientRequest::ChatClose{chat_id:source.id}).unwrap();
        assert!(actor.is_completed());assert!(!host.lock().unwrap().chat_restarts.contains_key(&source.id));
    }
}
