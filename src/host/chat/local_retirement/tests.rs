//! Actual Local process + actual Registry transaction failure/retry contracts.
use super::*;
use crate::{agent_execution::{Destination,HostedSession,Origin,Spec,Submit,Surface,Target},
    chat::{Command,Event,protocol::{Adapter,ProcessSpec,Step}},
    model::{LaunchRecipe,ProjectKind},workspace_model::ModelEdit};
struct NoPrompt;
impl Adapter for NoPrompt {
    fn initialize(&mut self)->Result<Step,String>{Ok(Step::default())}
    fn command(&mut self,_:Command)->Result<Step,String>{Err("No prompt/control is authorized by this close fixture".into())}
    fn receive(&mut self,_:serde_json::Value)->Result<Step,String>{Ok(Step::default())}
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
    Handle::start(spec,Box::new(NoPrompt),initial,store,None,|_|{}).unwrap()
}
fn record(source:Session,actor:Handle)->Record {
    Record{execution_instance:uuid::Uuid::new_v4(),console_recipe:None,paired_terminal:None,
        handoff_receipt:None,source_binding:None,session:source,handle:actor,source_process:None,
        subscribers:BTreeMap::from([(1,false)]),observations:BTreeMap::new(),
        completion_observed:false,returning_terminal:false}
}
impl Fixture {
    fn new()->Self {
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
        let actor=actor(&source.launch.recipe);
        {let mut state=host.lock().unwrap();state.install_model(registry);state.register_client(1);
            state.chats.insert(source.id,record(source.clone(),actor.clone()));}
        let registry=crate::host::agent_execution::registry(&host).unwrap();
        let fault=rusqlite::Connection::open(&db).unwrap();
        fault.execute_batch("CREATE TRIGGER fail_local_retirement BEFORE UPDATE OF closed ON agents WHEN NEW.closed=1 BEGIN SELECT RAISE(ABORT,'local retirement write denied'); END;").unwrap();
        Self{_directory:directory,host,source,actor,registry,agent:accepted.agent.id,fault}
    }
    fn pending(&self)->Arc<Pending>{self.host.lock().unwrap().local_chat_retirements[&self.source.id].clone()}
    fn assert_retained(&self){
        assert!(self.actor.is_completed(),"actual Local close must complete before the SQL fault");
        let pending=self.pending();assert!(pending.sealed());assert!(pending.snapshot().is_some());
        assert!(matches(&self.host.lock().unwrap(),&pending));
        assert!(!self.registry.lock().unwrap().agent(self.agent).unwrap().closed);
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
