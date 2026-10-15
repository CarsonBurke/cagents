//! Actor-local access to qualified durable metadata. Transcript hydration stays
//! in Store; only explicit user decisions and minimal annotations live here.
use crate::{account::scope::LaunchAccountScope,chat::{annotations::{self,ApprovalUpdate,ApprovalKey,OriginalError,OriginalPage},Event,Item,ToolStatus}};

pub(super) struct Authority {
    scope:Result<LaunchAccountScope,String>,
    session:Option<String>,
    latest_proven:Option<String>,
    unconfirmed_commands:Vec<String>,
    store:Option<annotations::storage::Store>,
    notice:Option<String>,
    retry_at:Option<std::time::Instant>,
    #[cfg(test)]
    root:Option<std::path::PathBuf>,
}
impl Authority {
    pub(super) fn new(scope:Result<LaunchAccountScope,String>)->Self{Self{scope,session:None,latest_proven:None,unconfirmed_commands:Vec::new(),store:None,notice:None,retry_at:None,#[cfg(test)] root:None}}
    fn pending_store(&self)->Result<annotations::storage::Store,String>{
        let scope=self.scope.as_ref().map_err(Clone::clone)?;
        #[cfg(test)]
        if let Some(root)=&self.root{return annotations::storage::Store::pending_for_test(root,scope);}
        annotations::storage::Store::pending_installation(scope)
    }
    /// Captures immutable operator bytes before any provider dispatch. Mutation
    /// of attachment paths happens only after the complete durable publication.
    pub(super) fn prepare(&mut self,command:&mut crate::chat::Command,cwd:&std::path::Path)->Result<Option<String>,OriginalError>{
        let crate::chat::Command::Prompt{text,attachments,..}=command else{return Ok(None);};
        if self.session.is_none() && self.unconfirmed_commands.len()>=64{return Err(OriginalError::TooLarge);}
        let mut pending=self.pending_store().map_err(OriginalError::Unavailable)?;
        let mut captured_text=pending.capture_text(text)?;
        let mut captures=attachments.iter().map(|attachment|pending.retain_attachment(&if attachment.path.is_absolute(){attachment.path.clone()}else{cwd.join(&attachment.path)})).collect::<Result<Vec<_>,_>>()?;
        let id=uuid::Uuid::new_v4().to_string();
        let prepared=pending.prepare_command(&id,self.session.as_deref(),&mut captured_text,&mut captures)?;
        if self.session.is_none(){self.unconfirmed_commands.push(id.clone());}
        for (attachment,capture) in attachments.iter_mut().zip(prepared.attachments){attachment.path=capture.path;}
        Ok(Some(id))
    }
    pub(super) fn abandon(&mut self,id:&str)->Result<(),String>{
        self.pending_store()?.abandon_command(id).map_err(|error|format!("{error:?}"))?;
        self.unconfirmed_commands.retain(|command|command!=id);Ok(())
    }
    pub(super) fn originals(&mut self,updates:Vec<annotations::OriginalUpdate>)->Option<String>{
        for update in updates {
            let result=match update {
                annotations::OriginalUpdate::Settings{command_id,settings}=>self.pending_store().and_then(|mut store|store.prepared_settings(&command_id,&settings).map_err(|error|format!("{error:?}"))),
                annotations::OriginalUpdate::Rejected{command_id}=>self.abandon(&command_id),
                annotations::OriginalUpdate::Accepted{command_id,session_id,user_id,turn_id,aliases}=>{
                    if self.session.as_deref()!=Some(session_id.as_str()){Err("Original acceptance does not match the confirmed provider conversation".into())}
                    else if let Some(store)=&mut self.store{store.accept_prepared(&command_id,&user_id,&turn_id,&aliases).map_err(|error|format!("{error:?}"))}
                    else{Err("Qualified original command storage is unavailable".into())}
                },
                annotations::OriginalUpdate::Latest{session_id,user_id}=>{
                    if self.session.as_deref()!=Some(session_id.as_str()){Err("Latest user belongs to another provider conversation".into())}else{self.latest_user(&user_id)}
                },
            };
            if let Err(error)=result{return self.warning(error);}
        }
        None
    }
    pub(super) fn original(&self,user:Option<&str>,offset:usize,limit:usize)->Result<OriginalPage,OriginalError>{
        let store=self.store.as_ref().ok_or_else(||OriginalError::Unavailable(self.notice.clone().or_else(||self.scope.as_ref().err().cloned()).unwrap_or_else(||"No qualified provider-confirmed original command is available".into())))?;
        let user=user.or(self.latest_proven.as_deref()).ok_or_else(||OriginalError::Unavailable("The provider's current latest user message has not been confirmed".into()))?;
        store.original(Some(user),offset,limit)
    }
    /// Call only with provider-proven current conversation ordering; history
    /// arrival and a persisted last annotation are not such evidence.
    pub(super) fn latest_user(&mut self,user:&str)->Result<(),String>{
        self.store.as_mut().ok_or("Qualified annotation storage is unavailable")?.latest_user(user)?;
        self.latest_proven=Some(user.to_owned());Ok(())
    }
    pub(super) fn apply(&mut self,confirmed:Option<&Option<String>>,updates:Vec<ApprovalUpdate>,events:&mut Vec<Event>)->Option<String>{
        if let Some(identity)=confirmed && self.session.as_ref()!=identity.as_ref(){
            self.session=identity.clone();self.latest_proven=None;self.store=None;self.notice=None;self.retry_at=None;
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
        // These IDs came from this exact actor's pre-dispatch capture. Session
        // confirmation attributes ownership without inventing user acceptance.
        while let Some(id)=self.unconfirmed_commands.last(){
            let session=self.session.as_deref().expect("qualified store has session");
            if let Err(error)=store.associate_command(id,session){return self.warning(format!("{error:?}"));}
            self.unconfirmed_commands.pop();
        }
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
        let root=annotations::storage::test_directory();let home=root.path().join("codex-home");std::fs::create_dir(&home).unwrap();
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
        let root=annotations::storage::test_directory();let home=root.path().join("claude-home");std::fs::create_dir(&home).unwrap();
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
        let root=annotations::storage::test_directory();let home=root.path().join("codex-home");std::fs::create_dir(&home).unwrap();
        let scope=LaunchAccountScope::resolve(crate::agent::Provider::Codex,root.path(),|key|(key=="CODEX_HOME").then(||home.as_os_str().to_owned())).unwrap();
        let storage=root.path().join("annotations");std::fs::write(&storage,b"not a directory").unwrap();let mut authority=Authority::new(Ok(scope));authority.root=Some(storage.clone());
        let confirmed=Some("confirmed-session".to_owned());assert!(authority.apply(Some(&confirmed),vec![],&mut vec![]).is_some());assert!(authority.store.is_none());
        assert!(authority.apply(Some(&confirmed),vec![],&mut vec![]).is_none(),"backoff must avoid repeated open attempts/warnings");
        std::fs::remove_file(storage).unwrap();authority.retry_at=Some(std::time::Instant::now());
        assert!(authority.apply(Some(&confirmed),vec![],&mut vec![]).is_none());assert!(authority.store.is_some(),"same session must recover once storage is available");
    }

    #[test]
    fn annotation_failure_does_not_replace_provider_error_or_failed_run_diagnostic(){
        use super::super::{Handle,Update};
        use crate::agent_execution::ProviderEvent;
        struct ProviderFailure(crate::model::RunId);
        impl Adapter for ProviderFailure {
            fn initialize(&mut self)->Result<Step,String>{Ok(Step::default())}
            fn command(&mut self,_:Command)->Result<Step,String>{Ok(Step{writes:vec![json!({})],..Step::default()})}
            fn receive(&mut self,_:Value)->Result<Step,String>{
                let execution=|event|Event::Execution{run:self.0,event};
                Ok(Step{events:vec![Event::Session{id:"confirmed-session".into(),title:None},execution(ProviderEvent::Started{key:"request".into()}),execution(ProviderEvent::Scope),Event::Error{message:"Provider overloaded: 529".into(),fatal:false},execution(ProviderEvent::Settled),execution(ProviderEvent::Finished{outcome:crate::notifications::Kind::Failed})],..Step::default()})
            }
        }
        let root=annotations::storage::test_directory();let home=root.path().join("codex-home");std::fs::create_dir(&home).unwrap();let scope=LaunchAccountScope::resolve(crate::agent::Provider::Codex,root.path(),|key|(key=="CODEX_HOME").then(||home.as_os_str().to_owned())).unwrap();
        let spec=protocol::ProcessSpec{execution:None,messages:None,unset_environment:Default::default(),transport:protocol::TransportSpec::JsonLines,program:"/bin/cat".into(),arguments:vec![],working_directory:root.path().to_owned(),environment:Default::default()};
        let mut initial=Step::default();let cache=super::super::preflight(&spec,&mut initial).unwrap();let (send,receive)=crossbeam_channel::bounded(1);
        let handle=Handle::start_inner(spec,Box::new(ProviderFailure(crate::model::RunId::new())),initial,cache,None,None,Ok(scope),move|update|{if let Update::Execution{update:crate::agent_execution::Update::Finished{result,..},..}=update{let _=send.try_send(result);}}).unwrap();
        let blocked=root.path().join("blocked-annotations");std::fs::write(&blocked,b"not a directory").unwrap();handle.0.annotations.lock().unwrap().root=Some(blocked);
        handle.command(Command::Compact).unwrap();let result=receive.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        assert_eq!(result.diagnostic.as_deref(),Some("Provider overloaded: 529"));
        assert_eq!(handle.snapshot(None).unwrap().error.as_deref(),Some("Provider overloaded: 529"));
        assert!(handle.0.annotations.lock().unwrap().notice.is_some(),"fixture must actually fail annotation initialization");
        handle.close().unwrap();
    }

    #[test]
    fn cold_latest_lookup_needs_current_provider_order_and_never_uses_an_older_original(){
        use annotations::{OriginalPrompt,OriginalSettings,Setting};
        let root=annotations::storage::test_directory();let home=root.path().join("codex-home");std::fs::create_dir(&home).unwrap();let scope=LaunchAccountScope::resolve(crate::agent::Provider::Codex,root.path(),|key|(key=="CODEX_HOME").then(||home.as_os_str().to_owned())).unwrap();
        let storage=root.path().join("annotations");let mut saved=annotations::storage::Store::for_test(&storage,&scope,"session").unwrap();
        let original=OriginalPrompt{user_id:"older-accepted-user".into(),turn_id:"older-turn".into(),settings:OriginalSettings{model:Setting::Value("confirmed-model".into()),reasoning:Setting::Value("low".into()),fast:Setting::Value(false),permission:Setting::Value("ask".into())},attachments:vec![],text_bytes:4};
        saved.accept(&original,"text",&[],&mut []).unwrap();saved.latest_user(&original.user_id).unwrap();drop(saved);
        let mut authority=Authority::new(Ok(scope));authority.root=Some(storage);assert!(authority.apply(Some(&Some("session".into())),vec![],&mut vec![]).is_none());
        assert!(matches!(authority.original(None,0,4),Err(OriginalError::Unavailable(_))));
        assert_eq!(authority.original(Some(&original.user_id),0,4).unwrap().text,"text");
        authority.latest_user(&original.user_id).unwrap();assert_eq!(authority.original(None,0,4).unwrap().text,"text");
        authority.latest_user("newer-unannotated-provider-user").unwrap();assert!(matches!(authority.original(None,0,4),Err(OriginalError::Unavailable(_))));
    }

    #[test]
    fn same_actor_prepare_releases_capture_leases_before_early_acceptance_and_scoped_deletion(){
        use annotations::{OriginalSettings,OriginalUpdate,Setting};
        let root=annotations::storage::test_directory();let home=root.path().join("codex-home");std::fs::create_dir(&home).unwrap();
        let scope=LaunchAccountScope::resolve(crate::agent::Provider::Codex,root.path(),|key|(key=="CODEX_HOME").then(||home.as_os_str().to_owned())).unwrap();
        let storage=root.path().join("annotations");let mut authority=Authority::new(Ok(scope));authority.root=Some(storage);
        std::fs::write(root.path().join("image.png"),b"original image").unwrap();
        let mut command=Command::Prompt{text:"original /command".into(),attachments:vec![crate::chat::Attachment{path:"image.png".into()}],mode:crate::chat::PromptMode::Send};
        let id=authority.prepare(&mut command,root.path()).unwrap().unwrap();
        let path=if let Command::Prompt{attachments,..}=&command{attachments[0].path.clone()}else{unreachable!()};assert!(path.is_absolute());
        authority.apply(Some(&Some("confirmed-session".into())),vec![],&mut vec![]);
        let settings=OriginalSettings{model:Setting::Value("reported".into()),reasoning:Setting::Value("low".into()),fast:Setting::Unsupported,permission:Setting::Value("ask".into())};
        assert!(authority.originals(vec![OriginalUpdate::Settings{command_id:id.clone(),settings},OriginalUpdate::Accepted{command_id:id,session_id:"confirmed-session".into(),user_id:"actual-user".into(),turn_id:"actual-turn".into(),aliases:vec![]}]).is_none(),"same actor must not retain capture flocks after prepare");
        assert_eq!(authority.original(Some("actual-user"),0,1024).unwrap().text,"original /command");assert!(path.exists());
        let mut second=Command::Prompt{text:"accepted but settings unavailable".into(),attachments:vec![],mode:crate::chat::PromptMode::Send};
        authority.session=None;let id=authority.prepare(&mut second,root.path()).unwrap().unwrap();
        let pending=authority.pending_store().unwrap().prepared(&id).unwrap();let pending_path=pending.text.path;
        authority.session=Some("confirmed-session".into());
        assert!(authority.originals(vec![OriginalUpdate::Accepted{command_id:id.clone(),session_id:"confirmed-session".into(),user_id:"actual-second-user".into(),turn_id:"actual-second-turn".into(),aliases:vec![]}]).is_some());
        assert_eq!(authority.pending_store().unwrap().prepared(&id).unwrap().expected_session.as_deref(),Some("confirmed-session"));
        authority.store.as_mut().unwrap().purge_for_test().unwrap();assert!(!path.exists());assert!(!pending_path.exists());
    }

    #[test]
    fn first_confirmed_session_attributes_only_this_actors_unaccepted_intents_for_cold_purge(){
        let root=annotations::storage::test_directory();let home=root.path().join("codex-home");std::fs::create_dir(&home).unwrap();
        let scope=LaunchAccountScope::resolve(crate::agent::Provider::Codex,root.path(),|key|(key=="CODEX_HOME").then(||home.as_os_str().to_owned())).unwrap();
        let storage=root.path().join("annotations");
        let mut unrelated=annotations::storage::Store::pending_for_test(&storage,&scope).unwrap();let mut text=unrelated.capture_text("other actor unknown intent").unwrap();let other=unrelated.prepare_command("other-command",None,&mut text,&mut []).unwrap();drop(text);drop(unrelated);
        let mut authority=Authority::new(Ok(scope.clone()));authority.root=Some(storage.clone());let mut command=Command::Prompt{text:"first operator input".into(),attachments:vec![],mode:crate::chat::PromptMode::Send};
        let id=authority.prepare(&mut command,root.path()).unwrap().unwrap();let own=authority.pending_store().unwrap().prepared(&id).unwrap();assert_eq!(own.expected_session,None);
        assert!(authority.apply(Some(&Some("actual-session".into())),vec![],&mut vec![]).is_none());
        assert_eq!(authority.pending_store().unwrap().prepared(&id).unwrap().expected_session.as_deref(),Some("actual-session"));
        assert!(matches!(authority.original(Some("unknown-user"),0,32),Err(OriginalError::Unavailable(_))),"Lifecycle attribution does not grant original acceptance");drop(authority);
        let mut cold=annotations::storage::Store::for_test(&storage,&scope,"actual-session").unwrap();cold.purge_for_test().unwrap();assert!(!own.text.path.exists());assert!(other.text.path.exists());assert!(cold.prepared("other-command").is_ok());
    }

}
