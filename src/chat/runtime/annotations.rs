//! Actor-local access to qualified durable metadata. Transcript hydration stays
//! in Store; only explicit user decisions and minimal annotations live here.
use crate::{account::scope::LaunchAccountScope,chat::{annotations::{self,ApprovalUpdate,ApprovalKey,OriginalError,OriginalPage},Event,Item,ToolStatus}};

pub(super) struct Authority {
    scope:Result<LaunchAccountScope,String>,
    session:Option<String>,
    store:Option<annotations::storage::Store>,
    notice:Option<String>,
    retry_at:Option<std::time::Instant>,
    #[cfg(test)]
    root:Option<std::path::PathBuf>,
}
impl Authority {
    pub(super) fn new(scope:Result<LaunchAccountScope,String>)->Self{Self{scope,session:None,store:None,notice:None,retry_at:None,#[cfg(test)] root:None}}
    pub(super) fn original(&self,user:Option<&str>,offset:usize,limit:usize)->Result<OriginalPage,OriginalError>{
        self.store.as_ref().ok_or_else(||OriginalError::Unavailable("No qualified provider-confirmed original command is available".into()))?.original(user,offset,limit)
    }
    pub(super) fn apply(&mut self,confirmed:Option<&Option<String>>,updates:Vec<ApprovalUpdate>,events:&mut Vec<Event>)->Option<String>{
        if let Some(identity)=confirmed && self.session.as_ref()!=identity.as_ref(){
            self.session=identity.clone();self.store=None;self.notice=None;self.retry_at=None;
        }
        if self.store.is_none() && self.retry_at.is_none_or(|at|std::time::Instant::now()>=at){
            if let (Ok(scope),Some(session))=(&self.scope,&self.session){
                #[cfg(test)]
                let opened=if let Some(root)=&self.root{annotations::storage::Store::for_test(root,scope,session)}else{annotations::storage::Store::installation(scope,session)};
                #[cfg(not(test))]
                let opened=annotations::storage::Store::installation(scope,session);
                match opened {
                    Ok(store)=>{self.store=Some(store);self.retry_at=None;self.notice=None;},
                    Err(error)=>{self.retry_at=Some(std::time::Instant::now()+std::time::Duration::from_secs(1));return self.warning(error);},
                }
            }
        }
        let Some(store)=&mut self.store else{return None;};
        for update in updates {
            if update.session_id.as_ref()!=self.session.as_ref(){return self.warning("An approval belongs to a different or unconfirmed conversation".into());}
            if let Err(error)=store.approval(&update.key,&update.request_id,update.decision){return self.warning(error);}
        }
        for event in events.iter_mut(){
            if let Err(error)=normalize(store,event){return self.warning(error);}
        }
        None
    }
    fn warning(&mut self,error:String)->Option<String>{
        if self.notice.as_ref()==Some(&error){None}else{self.notice=Some(error.clone());Some(format!("Conversation annotations unavailable: {error}"))}
    }
}
fn normalize_item(store:&annotations::storage::Store,agent:Option<&str>,item:&mut Item)->Result<(),String>{
    if item.content.status().is_some() && store.denied(&ApprovalKey{agent_id:agent.map(str::to_owned),item_id:item.id.clone()})? {
        item.content.set_status(ToolStatus::Denied);
    }
    Ok(())
}
pub(super) fn normalize(store:&annotations::storage::Store,event:&mut Event)->Result<(),String>{
    match event {
        Event::Item(item)=>normalize_item(store,None,item)?,
        Event::History{items,..}|Event::LatestHistory{items,..}=>for item in items{normalize_item(store,None,item)?;},
        Event::SubagentItem{id,item}=>normalize_item(store,Some(id),item)?,
        _=>{},
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::{Command,protocol::{self,Adapter,Step}};
    use serde_json::{Value,json};
    fn ingest(durable:&mut annotations::storage::Store,cache:&mut super::super::store::Store,adapter:&mut dyn Adapter,mut step:Step)->Vec<Event>{
        for update in adapter.take_approval_updates(){assert_eq!(update.session_id.as_deref(),Some("thread-exact"));durable.approval(&update.key,&update.request_id,update.decision).unwrap();}
        let mut forwarded=vec![];
        for event in &mut step.events{normalize(durable,event).unwrap();forwarded.extend(cache.apply(event.clone()).unwrap());}
        forwarded
    }
    fn request(thread:&str,n:usize,id:usize)->Value{
        json!({"id":id,"method":"item/commandExecution/requestApproval","params":{"threadId":thread,"turnId":"turn","itemId":format!("tool-{n}"),"command":"git status","cwd":"/repo","availableDecisions":["accept","decline"]}})
    }
    fn completed(thread:&str,n:usize)->Value{
        json!({"method":"item/completed","params":{"threadId":thread,"turnId":"turn","item":{"id":format!("tool-{n}"),"type":"commandExecution","command":"git status","cwd":"/repo","status":"failed","aggregatedOutput":"generic replayed failure"}}})
    }
    fn cached_status(cache:&mut super::super::store::Store,id:&str)->ToolStatus{
        cache.latest();loop{let page=cache.window().unwrap();if let Some(item)=page.items.iter().find(|item|item.id==id){return item.content.status().unwrap();}assert!(page.has_earlier,"missing {id}");assert!(cache.earlier().unwrap());}
    }
    #[test]
    fn live_codex_replay_after_approval_eviction_and_cold_reconstruction_stays_denied(){
        let root=tempfile::tempdir().unwrap();let home=root.path().join("codex-home");std::fs::create_dir(&home).unwrap();
        let scope=LaunchAccountScope::resolve(crate::agent::Provider::Codex,root.path(),|key|(key=="CODEX_HOME").then(||home.as_os_str().to_owned())).unwrap();
        let annotations=root.path().join("annotations");
        let mut durable=annotations::storage::Store::for_test(&annotations,&scope,"thread-exact").unwrap();
        let mut cache=super::super::store::Store::new().unwrap();let mut adapter=protocol::codex::annotation_test_adapter(true);
        for n in 0..400{
            let step=adapter.receive(request("thread-exact",n,n+1)).unwrap();ingest(&mut durable,&mut cache,adapter.as_mut(),step);
            let step=adapter.command(Command::Answer{request_id:(n+1).to_string(),action:"decision-1".into(),answers:vec![]}).unwrap();ingest(&mut durable,&mut cache,adapter.as_mut(),step);
        }
        let step=adapter.receive(completed("thread-exact",0)).unwrap();
        assert!(step.events.iter().any(|event|matches!(event,Event::Item(item) if item.id=="turn/tool-0"&&item.content.status()==Some(ToolStatus::Failed))),"fixture must actually evict the adapter's denial");
        ingest(&mut durable,&mut cache,adapter.as_mut(),step);assert_eq!(cached_status(&mut cache,"turn/tool-0"),ToolStatus::Denied);
        let step=adapter.receive(request("child-exact",0,900)).unwrap();ingest(&mut durable,&mut cache,adapter.as_mut(),step);
        let step=adapter.command(Command::Answer{request_id:"900".into(),action:"decision-1".into(),answers:vec![]}).unwrap();ingest(&mut durable,&mut cache,adapter.as_mut(),step);
        drop(adapter);drop(cache);drop(durable);
        let mut durable=annotations::storage::Store::for_test(&annotations,&scope,"thread-exact").unwrap();let mut cache=super::super::store::Store::new().unwrap();let mut adapter=protocol::codex::annotation_test_adapter(true);
        let step=adapter.receive(completed("thread-exact",0)).unwrap();ingest(&mut durable,&mut cache,adapter.as_mut(),step);assert_eq!(cached_status(&mut cache,"turn/tool-0"),ToolStatus::Denied);
        let step=adapter.receive(completed("child-exact",0)).unwrap();let child=ingest(&mut durable,&mut cache,adapter.as_mut(),step);assert!(child.iter().any(|event|matches!(event,Event::SubagentItem{id,item} if id=="child-exact"&&item.content.status()==Some(ToolStatus::Denied))));
        // Replaying the same live RPC does not establish a new action.
        let step=adapter.receive(request("thread-exact",0,1)).unwrap();ingest(&mut durable,&mut cache,adapter.as_mut(),step);assert_eq!(cached_status(&mut cache,"turn/tool-0"),ToolStatus::Denied);
        let step=adapter.command(Command::Answer{request_id:"1".into(),action:"decision-0".into(),answers:vec![]}).unwrap();ingest(&mut durable,&mut cache,adapter.as_mut(),step);assert_eq!(cached_status(&mut cache,"turn/tool-0"),ToolStatus::Denied);
        // A distinct actual Awaiting request followed by Allow releases denial;
        // the subsequent real provider failure must remain a failure.
        let step=adapter.receive(request("thread-exact",0,901)).unwrap();ingest(&mut durable,&mut cache,adapter.as_mut(),step);assert_eq!(cached_status(&mut cache,"turn/tool-0"),ToolStatus::AwaitingApproval);
        let step=adapter.command(Command::Answer{request_id:"901".into(),action:"decision-0".into(),answers:vec![]}).unwrap();ingest(&mut durable,&mut cache,adapter.as_mut(),step);
        let step=adapter.receive(completed("thread-exact",0)).unwrap();ingest(&mut durable,&mut cache,adapter.as_mut(),step);assert_eq!(cached_status(&mut cache,"turn/tool-0"),ToolStatus::Failed);
    }
    #[test]
    fn first_claude_control_request_confirms_scope_before_awaiting_and_denied_capture(){
        let root=tempfile::tempdir().unwrap();let home=root.path().join("claude-home");std::fs::create_dir(&home).unwrap();
        let scope=LaunchAccountScope::resolve(crate::agent::Provider::Claude,root.path(),|key|(key=="CLAUDE_CONFIG_DIR").then(||home.as_os_str().to_owned())).unwrap();
        let mut authority=Authority::new(Ok(scope.clone()));authority.root=Some(root.path().join("annotations"));
        let mut adapter=protocol::claude::annotation_test_adapter();
        let session="00000000-0000-4000-8000-000000000001";
        let frame=json!({"type":"control_request","session_id":session,"request_id":"first-request","request":{"subtype":"can_use_tool","tool_name":"Bash","tool_use_id":"tool-exact","input":{"command":"git status"}}});
        let mut step=adapter.receive(frame.clone()).unwrap();
        let confirmed=step.events.iter().find_map(|event|if let Event::Session{id,..}=event{Some(Some(id.clone()))}else{None});assert_eq!(confirmed,Some(Some(session.into())));
        assert!(authority.apply(confirmed.as_ref(),adapter.take_approval_updates(),&mut step.events).is_none());
        let mut answer=adapter.command(Command::Answer{request_id:"first-request".into(),action:"deny".into(),answers:vec![]}).unwrap();
        assert!(authority.apply(None,adapter.take_approval_updates(),&mut answer.events).is_none());
        let key=ApprovalKey{agent_id:None,item_id:"tool-exact".into()};assert!(authority.store.as_ref().unwrap().denied(&key).unwrap());
        drop(authority);drop(adapter);
        let mut cold=Authority::new(Ok(scope));cold.root=Some(root.path().join("annotations"));let mut adapter=protocol::claude::annotation_test_adapter();let mut step=adapter.receive(frame).unwrap();
        let confirmed=Some(session.to_owned());assert!(cold.apply(Some(&confirmed),adapter.take_approval_updates(),&mut step.events).is_none());
        assert!(step.events.iter().any(|event|matches!(event,Event::Item(item) if item.content.status()==Some(ToolStatus::Denied))));
    }
    #[test]
    fn transient_open_failure_retries_same_qualified_session_after_backoff(){
        let root=tempfile::tempdir().unwrap();let home=root.path().join("codex-home");std::fs::create_dir(&home).unwrap();
        let scope=LaunchAccountScope::resolve(crate::agent::Provider::Codex,root.path(),|key|(key=="CODEX_HOME").then(||home.as_os_str().to_owned())).unwrap();
        let storage=root.path().join("annotations");std::fs::write(&storage,b"not a directory").unwrap();let mut authority=Authority::new(Ok(scope));authority.root=Some(storage.clone());
        let confirmed=Some("confirmed-session".to_owned());assert!(authority.apply(Some(&confirmed),vec![],&mut vec![]).is_some());assert!(authority.store.is_none());
        assert!(authority.apply(Some(&confirmed),vec![],&mut vec![]).is_none(),"backoff must avoid repeated open attempts/warnings");
        std::fs::remove_file(storage).unwrap();authority.retry_at=Some(std::time::Instant::now());
        assert!(authority.apply(Some(&confirmed),vec![],&mut vec![]).is_none());assert!(authority.store.is_some(),"same session must recover once storage is available");
    }

}
