//! Actual Local process + actual Registry transaction failure/retry contracts.
use super::*;
use crate::{agent_execution::{Destination,HostedSession,Origin,Spec,Submit,Surface,Target},
    chat::{Command,Event,protocol::{Adapter,ProcessSpec,Step}},
    model::{LaunchRecipe,ProjectKind},workspace_model::ModelEdit};
struct NoPrompt { confirm:bool }
impl Adapter for NoPrompt {
    fn initialize(&mut self)->Result<Step,String>{Ok(Step::default())}
    fn command(&mut self,command:Command)->Result<Step,String>{
        if self.confirm && matches!(command,Command::Refresh) {
            Ok(Step{writes:vec![serde_json::json!({"confirm":true})],events:Vec::new()})
        } else { Err("No prompt/control is authorized by this close fixture".into()) }
    }
    fn receive(&mut self,_:serde_json::Value)->Result<Step,String>{Ok(Step{writes:Vec::new(),events:if self.confirm {
        vec![Event::Session{id:"confirmed-by-original-cat-frame".into(),title:None}]
    } else {Vec::new()}})}
}
struct Fixture {
    _directory:tempfile::TempDir, host:SharedHost, source:Session, actor:Handle,
    registry:super::super::super::agent_execution::SharedRegistry,
    agent:crate::model::AgentId, fault:rusqlite::Connection,
}
fn actor(recipe:&LaunchRecipe)->Handle {
    let spec=ProcessSpec::from(recipe);
    let mut initial=Step{writes:Vec::new(),events:vec![Event::Ready(true)]};
    let store=chat::runtime::preflight(&spec,&mut initial).unwrap();
    Handle::start(spec,Box::new(NoPrompt{confirm:false}),initial,store,None,|_|{}).unwrap()
}
fn record(source:Session,actor:Handle)->Record {
    Record{execution_instance:uuid::Uuid::new_v4(),console_recipe:None,paired_terminal:None,
        handoff_receipt:None,source_binding:None,session:source,handle:actor,source_process:None,
        subscribers:BTreeMap::from([(1,false)]),observations:BTreeMap::new(),
        completion_observed:false,returning_terminal:false}
}
impl Fixture {
    fn new()->Self {Self::observed(None)}
    fn observed(barrier:Option<(std::sync::mpsc::Sender<()>,std::sync::mpsc::Receiver<()>)>)->Self {
        let directory=tempfile::tempdir().unwrap();
        let db=directory.path().join("registry.sqlite3");
        let mut registry=crate::host::model::Registry::at(&db).unwrap();
        registry.apply(&ModelEdit::OpenProject{repository:directory.path().into(),checkout:None,
            primary_checkout:None,name:"Local close custody".into(),kind:ProjectKind::Folder}).unwrap();
        let accepted=registry.submit_agent(&Origin::User,&Submit{name:None,
            destination:Destination::New(Spec{provider:Provider::Omp,account:None,
                target:Target{repository:directory.path().into(),directory:directory.path().into(),placement:None},
                surface:Surface::Chat,model:None,permission_mode:None,reasoning_effort:None}),
            prompt:"This request remains unaccepted by the provider".into(),idempotency_key:"local-retirement".into()},1).unwrap();
        let host=crate::host::service::test_host();
        let source=Session{id:TerminalId::new(),launch:chat::Launch{provider:Provider::Omp,
            recipe:LaunchRecipe{program:"/bin/cat".into(),arguments:Vec::new(),working_directory:directory.path().into(),
                environment:Default::default(),unset_environment:Default::default()},session_id:None},
            proof:Some(host.lock().unwrap().proof().clone()),terminal:None,owned_codex_listener:None,
            native_continuity:Default::default(),archive_report:None};
        registry.bind_agent(accepted.agent.id,None,HostedSession::Chat(source.clone())).unwrap();
        let instance=uuid::Uuid::new_v4();
        let confirm=barrier.is_some();let barrier=std::sync::Mutex::new(barrier);
        let weak=Arc::downgrade(&host);let id=source.id;
        let spec=ProcessSpec::from(&source.launch.recipe);
        let mut initial=Step{writes:Vec::new(),events:vec![Event::Ready(true)]};
        let store=chat::runtime::preflight(&spec,&mut initial).unwrap();
        let actor=Handle::start(spec,Box::new(NoPrompt{confirm}),initial,store,None,move|update|{
            if matches!(&update,Update::Identity) {
                let wait=barrier.lock().unwrap().take();
                if let Some((entered,release))=wait {entered.send(()).unwrap();release.recv_timeout(Duration::from_secs(5)).unwrap();}
            }
            super::super::changed_from(&weak,id,Some(instance),update);
        }).unwrap();
        let registration=registry.install_session_observer(&HostedSession::Chat(source.clone()),instance,None,None,None).unwrap();
        {let mut state=host.lock().unwrap();state.install_model(registry);state.register_client(1);
            state.publish_chat_observer(source.id,instance,registration.stamp());
            let mut issued=record(source.clone(),actor.clone());issued.execution_instance=instance;
            state.chats.insert(source.id,issued);}
        let registry=crate::host::agent_execution::registry(&host).unwrap();
        let fault=rusqlite::Connection::open(&db).unwrap();
        fault.execute_batch("CREATE TRIGGER fail_local_retirement BEFORE UPDATE OF closed ON agents WHEN NEW.closed=1 BEGIN SELECT RAISE(ABORT,'local retirement write denied'); END;").unwrap();
        Self{_directory:directory,host,source,actor,registry,agent:accepted.agent.id,fault}
    }
    fn pending(&self)->Arc<Pending>{self.host.lock().unwrap().local_chat_retirements[&self.source.id].clone()}
    fn assert_retained(&self){
        assert!(self.actor.is_completed(),"actual Local close must complete before the SQL fault");
        let pending=self.pending();assert!(pending.sealed());assert!(pending.snapshot().is_some());
        assert!(matches_owner(&self.host.lock().unwrap(),&pending));
        assert!(!self.registry.lock().unwrap().agent(self.agent).unwrap().closed);
        assert!(matches!(self.host.lock().unwrap().shutdown_preflight(Instant::now()),
            Err(crate::terminal_ipc::ShutdownOutcome::OwnershipUncertain)));
    }
    fn allow_commit(&self){self.fault.execute_batch("DROP TRIGGER fail_local_retirement").unwrap();}
    fn assert_retired(&self){
        let state=self.host.lock().unwrap();assert!(!state.chats.contains_key(&self.source.id));
        assert!(!state.local_chat_retirements.contains_key(&self.source.id));drop(state);
        assert!(self.registry.lock().unwrap().agent(self.agent).unwrap().closed);
    }
}
#[test]
fn local_chat_close_retains_positive_owner_across_actual_sql_failure_and_retry(){
    if chat::runtime::isolated_backend_contract("host::chat::local_retirement::tests::local_chat_close_retains_positive_owner_across_actual_sql_failure_and_retry"){return;}
    let f=Fixture::new();
    let refused=super::super::process(&f.host,1,ClientRequest::ChatClose{chat_id:f.source.id}).unwrap_err();
    assert!(refused.message.contains("local retirement write denied"),"{refused:?}");f.assert_retained();
    assert!(require_open(&f.host.lock().unwrap(),f.source.id).is_err());
    {
        use crate::project_recovery::{ProjectInstance,SessionClose};
        use crate::model::{TerminalSession,TerminalContinuity};
        let own=f._directory.path().to_owned();let other=own.join("unrelated-project");std::fs::create_dir(&other).unwrap();
        let model=f.registry.lock().unwrap().apply(&ModelEdit::OpenProject{repository:other.clone(),checkout:None,primary_checkout:None,name:"Independent".into(),kind:ProjectKind::Folder}).unwrap().model;
        let project=ProjectInstance{registry:model.identity,repository:own.clone(),ordinal:model.project(&own).unwrap().ordinal};
        let foreign=ProjectInstance{registry:model.identity,repository:other.clone(),ordinal:model.project(&other).unwrap().ordinal};
        struct Terminals(Vec<crate::terminal::TerminalHandle>);
        impl Drop for Terminals{fn drop(&mut self){for terminal in &self.0{let _=terminal.shutdown();}}}
        let mut terminals=Terminals(Vec::new());
        let mut spawn=|path:&std::path::Path| {
            let mut recipe=LaunchRecipe::shell(path.to_owned()).unwrap();recipe.program="/bin/cat".into();recipe.arguments.clear();
            let session=TerminalSession{id:TerminalId::new(),launch:recipe.clone(),owned_codex_listener:None,recovery_origin:None,predecessor:None,transcript_path:None,resume_hint:None,continuity:TerminalContinuity::RelaunchAvailable{recipe,last_exit:None}};
            let worker=crate::terminal::TerminalHandle::spawn(session.clone(),Default::default(),|_|{}).unwrap();
            let descriptor={let mut state=f.host.lock().unwrap();state.reserve_spawn(session.id).unwrap();state.finish_spawn(1,session,worker.clone(),true).unwrap()};
            terminals.0.push(worker);SessionClose{source:HostedSession::Terminal(descriptor.session),execution_instance:Some(descriptor.execution_instance),provider:None}
        };
        let healthy=spawn(&own);let independent=spawn(&other);let pending=f.pending();
        let inventory=crate::host::client::process_request(&f.host,1,ClientRequest::ProjectCloseInventory{project:project.clone()}).unwrap_err();
        assert!(inventory.message.contains("retirement"),"{inventory:?}");
        let remove=crate::host::client::process_request(&f.host,1,ClientRequest::CloseProjectGuarded{project,sessions:vec![healthy.clone()]}).unwrap_err();
        assert!(remove.message.contains("retirement"),"{remove:?}");
        assert_eq!(f.host.lock().unwrap().read_model().unwrap(),model);
        assert!(!terminals.0[0].is_completed()&&!terminals.0[1].is_completed());
        assert!(Arc::ptr_eq(&f.pending(),&pending));assert!(!f.registry.lock().unwrap().agent(f.agent).unwrap().closed);
        assert!(f.host.lock().unwrap().project_removals.lock().unwrap().is_empty());
        assert!(crate::host::client::process_request(&f.host,1,ClientRequest::Input{terminal_id:independent.source.id(),bytes:b"independent of SQL retry\n".to_vec()}).is_ok());
        let result=crate::host::client::process_request(&f.host,1,ClientRequest::CloseProjectGuarded{project:foreign,sessions:vec![independent]}).unwrap();
        assert!(matches!(result.0,ServerResponse::WorkspaceModel{..}));assert!(!terminals.0[0].is_completed()&&terminals.0[1].is_completed());
        assert!(Arc::ptr_eq(&f.pending(),&pending));assert!(!f.registry.lock().unwrap().agent(f.agent).unwrap().closed);
        let HostedSession::Terminal(source)=healthy.source else{unreachable!()};
        let result=crate::host::client::process_request(&f.host,1,ClientRequest::CloseTerminalSourceChecked{source,execution_instance:healthy.execution_instance.unwrap(),allow_active:true}).unwrap();
        assert!(matches!(result.0,ServerResponse::TerminalCloseChecked{outcome:crate::terminal_ipc::TerminalCloseResult::Closed,..}));
    }
    let before=f.pending().snapshot().unwrap();f.allow_commit();
    let ServerResponse::ChatClosed{snapshot,..}=super::super::process(&f.host,1,ClientRequest::ChatClose{chat_id:f.source.id}).unwrap() else{panic!("exact close retry must publish ChatClosed")};
    assert_eq!(snapshot.generation,before.generation);assert_eq!(snapshot.session_id,before.session_id);f.assert_retired();
}
#[test]
fn local_terminal_actor_close_retries_sealed_owner_without_reclosing_process(){
    if chat::runtime::isolated_backend_contract("host::chat::local_retirement::tests::local_terminal_actor_close_retries_sealed_owner_without_reclosing_process"){return;}
    let f=Fixture::new();
    let refused=super::super::close_terminal_actor(&f.host,f.source.id,&f.actor).unwrap_err();
    assert!(refused.message.contains("local retirement write denied"),"{refused:?}");f.assert_retained();
    f.allow_commit();super::super::close_terminal_actor(&f.host,f.source.id,&f.actor).unwrap();f.assert_retired();
}
#[test]
fn sealed_local_retirement_preserves_newer_actor_and_enriched_source(){
    if chat::runtime::isolated_backend_contract("host::chat::local_retirement::tests::sealed_local_retirement_preserves_newer_actor_and_enriched_source"){return;}
    let f=Fixture::new();assert!(super::super::close_terminal_actor(&f.host,f.source.id,&f.actor).is_err());f.assert_retained();
    let pending=f.pending();f.allow_commit();
    {let mut state=f.host.lock().unwrap();state.chats.get_mut(&f.source.id).unwrap().session.launch.session_id=Some("newly-learned-thread".into());}
    assert!(commit(&f.host,&pending).is_err());
    assert_eq!(f.host.lock().unwrap().chats[&f.source.id].session.launch.session_id.as_deref(),Some("newly-learned-thread"));
    let successor=actor(&f.source.launch.recipe);
    let original=f.host.lock().unwrap().chats.insert(f.source.id,record(f.source.clone(),successor.clone())).unwrap();
    assert!(commit(&f.host,&pending).is_err());assert!(!successor.is_completed());
    assert!(f.host.lock().unwrap().chats[&f.source.id].handle.same_actor(&successor));
    assert!(!f.registry.lock().unwrap().agent(f.agent).unwrap().closed);
    let displaced={let mut state=f.host.lock().unwrap();let mut original=original;original.session=f.source.clone();state.chats.insert(f.source.id,original)};
    drop(displaced);successor.close().unwrap();
    commit(&f.host,&pending).unwrap();f.assert_retired();
}

#[test]
fn original_local_late_thread_callback_survives_failed_retirement_and_exact_retry(){
    if chat::runtime::isolated_backend_contract("host::chat::local_retirement::tests::original_local_late_thread_callback_survives_failed_retirement_and_exact_retry"){return;}
    let (entered,waiting)=std::sync::mpsc::channel();let (release,released)=std::sync::mpsc::channel();
    let f=Fixture::observed(Some((entered,released)));
    f.actor.command(Command::Refresh).unwrap();
    waiting.recv_timeout(Duration::from_secs(5)).unwrap();
    let closing_host=f.host.clone();let id=f.source.id;
    let closing=thread::spawn(move||super::super::process(&closing_host,1,ClientRequest::ChatClose{chat_id:id}));
    let deadline=Instant::now()+Duration::from_secs(5);
    loop {
        if f.host.lock().unwrap().local_chat_retirements.contains_key(&id){break;}
        assert!(Instant::now()<deadline,"actual close did not reserve before original callback release");
        thread::sleep(Duration::from_millis(5));
    }
    release.send(()).unwrap();
    let refused=closing.join().unwrap().unwrap_err();
    assert!(refused.message.contains("local retirement write denied"),"{refused:?}");f.assert_retained();
    assert_eq!(f.host.lock().unwrap().chats[&id].session.launch.session_id.as_deref(),Some("confirmed-by-original-cat-frame"));
    assert_eq!(f.pending().snapshot().unwrap().session_id.as_deref(),Some("confirmed-by-original-cat-frame"));
    f.allow_commit();
    assert!(matches!(super::super::process(&f.host,1,ClientRequest::ChatClose{chat_id:id}).unwrap(),ServerResponse::ChatClosed{..}));
    f.assert_retired();
}
