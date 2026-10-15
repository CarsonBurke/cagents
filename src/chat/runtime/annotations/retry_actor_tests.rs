//! Real actor/stdio fixtures for held Retry admission and provider-error priority.
//! This models provider wire; it is not a live-provider or native UI receipt.
use super::*;
use crate::chat::runtime::Handle;
use std::time::{Duration, Instant};
fn wait(mut condition:impl FnMut()->bool) {
    let deadline=Instant::now()+Duration::from_secs(5);
    while !condition() { assert!(Instant::now()<deadline,"Retry actor fixture did not reach its fence");std::thread::sleep(Duration::from_millis(5)); }
}
#[test]
fn actual_actor_held_retry_preserves_idle_turn_root_error_and_reconciles_cancellation() {
    for mode in ["dispatch","interrupt","refusal","timeout","native","close","eof","crash","deny"] {
        let root=annotations::storage::test_directory();
        let home=root.path().join("claude-home");std::fs::create_dir(&home).unwrap();
        let scope=LaunchAccountScope::resolve(crate::agent::Provider::Claude,root.path(),|key|(key=="CLAUDE_CONFIG_DIR").then(||home.as_os_str().to_owned())).unwrap();
        let script=root.path().join("retry-provider.py");
        std::fs::write(&script,r#"import sys,os,json,select,pathlib
root=pathlib.Path(sys.argv[1]);mode=sys.argv[2];session='00000000-0000-4000-8000-000000000001'
model='reported-model';effort='low';fast=False;permission='default';pending=None;data=b'';users=0;initialized=False
def emit(frame): print(json.dumps(frame),flush=True)
def reply(request,payload,success=True): emit({'type':'control_response','response':{'subtype':'success' if success else 'error','request_id':request['request_id'],'response':payload}})
while True:
 if pending and (root/'release').exists():
  if mode in ('eof','crash'):
   if mode=='crash': print('original-provider-crash',file=sys.stderr,flush=True)
   sys.exit(7 if mode=='crash' else 0)
  if mode=='native': emit({'type':'system','subtype':'session_state_changed','session_id':session,'state':'running'})
  reply(pending,{},mode!='refusal');pending=None;(root/'late-setter-sent').touch()
 if not select.select([0],[],[],0.01)[0]: continue
 block=os.read(0,65536)
 if not block: break
 data+=block
 while b'\n' in data:
  line,data=data.split(b'\n',1);frame=json.loads(line)
  if frame.get('type')=='control_request':
   request=frame['request'];subtype=request['subtype']
   if subtype=='initialize':
    reply(frame,{'claude_code_version':'2.1.289','current_permission_mode':permission,'fast_mode_state':'on' if fast else 'off','session_state':'running' if mode=='native' and users else 'idle'})
    if not initialized:
     initialized=True;emit({'type':'system','subtype':'init','session_id':session,'model':model,'effort':effort,'permissionMode':permission,'fast_mode_state':'on' if fast else 'off'})
   elif subtype=='get_settings': reply(frame,{'applied':{'model':model,'effort':effort}})
   elif subtype=='set_model':
    model=request['model'];pending=frame
    emit({'type':'system','subtype':'session_state_changed','session_id':session,'state':'idle'})
    if mode=='deny': emit({'type':'control_request','request_id':'native-approval','session_id':session,'request':{'subtype':'can_use_tool','tool_name':'Bash','tool_use_id':'late-tool','input':{'command':'must be denied'}}})
    (root/'setter-seen').touch()
   elif subtype=='apply_flag_settings':
    settings=request['settings'];effort=settings.get('effortLevel',effort);fast=settings.get('fastMode',fast);reply(frame,{})
   elif subtype=='set_permission_mode': permission=request['mode'];reply(frame,{})
   else: reply(frame,{})
  elif frame.get('type')=='control_response':
   (root/'denial-response.json').write_text(json.dumps(frame))
  elif frame.get('type')=='user':
   users+=1
   with (root/'users.jsonl').open('a') as out: out.write(json.dumps(frame)+'\n')
   emit({'type':'user','uuid':frame['uuid'],'session_id':session,'parent_tool_use_id':None,'message':{'id':'echo-'+frame['uuid'],'role':'user','content':frame['message']['content']}})
   emit({'type':'command_lifecycle','session_id':session,'command_uuid':frame['uuid'],'state':'started'})
   if users==1:
    emit({'type':'result','subtype':'error_during_execution','uuid':'independent-result-1','session_id':session,'is_error':True,'errors':['529 Provider overloaded'],'terminal_reason':'model_error'})
   else: emit({'type':'result','subtype':'success','uuid':'independent-result-2','session_id':session,'is_error':False,'result':'retried','terminal_reason':'completed'})
   emit({'type':'command_lifecycle','session_id':session,'command_uuid':frame['uuid'],'state':'completed'})
   if users==1:
    model='changed-model';effort='high';fast=True;permission='bypassPermissions'
    emit({'type':'system','subtype':'init','session_id':session,'model':model,'effort':effort,'permissionMode':permission,'fast_mode_state':'on'});(root/'first-done').touch()
"#).unwrap();
        let spec=protocol::ProcessSpec {execution:None,messages:None,unset_environment:Default::default(),transport:protocol::TransportSpec::JsonLines,program:"python3".into(),arguments:vec![script.to_string_lossy().into_owned(),root.path().to_string_lossy().into_owned(),mode.into()],working_directory:root.path().to_owned(),environment:Default::default()};
        let timeout=if mode=="timeout" {Duration::from_millis(100)}else{Duration::from_secs(3)};
        let mut adapter=protocol::claude::annotation_test_retry_adapter(timeout);
        let mut initial=adapter.initialize().unwrap();let cache=super::super::preflight(&spec,&mut initial).unwrap();
        let handle=Handle::start_inner(spec,adapter,initial,cache,None,None,Ok(scope),|_|{}).unwrap();
        handle.0.annotations.lock().unwrap().root=Some(root.path().join("annotations"));
        wait(||handle.snapshot(None).unwrap().ready);
        handle.command(Command::Prompt {text:"exact original input".into(),attachments:vec![],mode:crate::chat::PromptMode::Send}).unwrap();
        wait(||root.path().join("first-done").exists()&&handle.snapshot(None).is_some_and(|state|!state.busy&&state.error.as_deref().is_some_and(|error|error.contains("529"))));
        let before=handle.snapshot(None).unwrap();
        let original=handle.0.annotations.lock().unwrap().original(None,0,4).unwrap().original;
        assert!(original.settings.is_exact());
        handle.command(Command::PromptOriginal {session_id:before.session_id.clone().unwrap(),original,turn_generation:before.turn_generation}).unwrap();
        wait(||root.path().join("setter-seen").exists()&&handle.snapshot(None).unwrap().original_retry_pending);
        let held=handle.snapshot(None).unwrap();
        assert!(!held.busy,"holding settings is not a native provider turn");
        assert_eq!(held.error,before.error);assert_eq!(held.turn_generation,before.turn_generation);
        assert_eq!(held.turn_started_at,before.turn_started_at);
        if mode!="deny" {assert_eq!(held.finished_at,before.finished_at);}
        assert_eq!(std::fs::read_to_string(root.path().join("users.jsonl")).unwrap().lines().count(),1);
        if mode=="close" {
            handle.close().unwrap();assert_eq!(handle.snapshot(None).unwrap().error,before.error);continue;
        }
        if mode=="interrupt" {handle.command(Command::Interrupt).unwrap();}
        if mode=="deny" {
            wait(||handle.snapshot(None).unwrap().requests.iter().any(|request|request.id=="native-approval"));
            handle.command(Command::Answer {request_id:"native-approval".into(),action:"deny".into(),answers:vec![]}).unwrap();
            wait(||root.path().join("denial-response.json").exists());
            let denial:Value=serde_json::from_slice(&std::fs::read(root.path().join("denial-response.json")).unwrap()).unwrap();
            assert_eq!(denial["response"]["request_id"],"native-approval");
            assert_eq!(denial["response"]["response"]["behavior"],"deny");
            assert!(!root.path().join("release").exists(),"Deny must reach provider before any settings setter receipt");
        }
        if mode=="timeout" {
            wait(||handle.snapshot(None).unwrap().original_retry_diagnostic.as_deref().is_some_and(|detail|detail.contains("timed out")));
            assert!(handle.command(Command::Refresh).is_err(),"unsettled setter cannot be read back prematurely");
        }
        if !matches!(mode,"dispatch"|"eof"|"crash") {
            assert!(handle.command(Command::Prompt {text:"must not run under partial settings".into(),attachments:vec![],mode:crate::chat::PromptMode::Queue}).is_err());
        }
        std::fs::write(root.path().join("release"),b"go").unwrap();
        if matches!(mode,"eof"|"crash") {
            wait(||handle.snapshot(None).unwrap().exited);
            let exited=handle.snapshot(None).unwrap();
            assert!(!exited.original_retry_pending);assert!(exited.original_retry_diagnostic.is_none());
            assert_eq!(std::fs::read_to_string(root.path().join("users.jsonl")).unwrap().lines().count(),1);
            let authority=handle.0.annotations.lock().unwrap();
            let pending=authority.pending_store().unwrap();
            assert_eq!(pending.pending_count_for_test().unwrap(),0,"exact never-written Retry intent must retire on unexpected actor termination");
            drop(pending);drop(authority);handle.close().unwrap();continue;
        }
        wait(||root.path().join("late-setter-sent").exists());
        if mode=="timeout" {
            wait(||handle.snapshot(None).unwrap().original_retry_diagnostic.as_deref().is_some_and(|detail|detail.contains("setter settled")));
            handle.command(Command::Refresh).unwrap();
        }
        wait(||!handle.snapshot(None).unwrap().original_retry_pending);
        let after=handle.snapshot(None).unwrap();
        let users=std::fs::read_to_string(root.path().join("users.jsonl")).unwrap().lines().count();
        if mode=="dispatch" {
            wait(||std::fs::read_to_string(root.path().join("users.jsonl")).unwrap().lines().count()==2);
        } else {
            assert_eq!(users,1,"cancelled original and refused newer input must remain unsent");
            if mode=="native" {assert!(after.busy);assert!(after.turn_generation>before.turn_generation);}
            else {
                assert_eq!(after.error,before.error);assert_eq!(after.turn_generation,before.turn_generation);
                assert_eq!(after.turn_started_at,before.turn_started_at);
                if mode!="deny" {assert_eq!(after.finished_at,before.finished_at);}
                assert_eq!(after.selected_model.as_deref(),Some("reported-model"));
                assert_eq!(after.reasoning.as_deref(),Some("high"));
                assert_eq!(after.permission_mode.as_deref(),Some("bypassPermissions"));
                assert!(after.original_retry_diagnostic.is_some());
            }
        }
        handle.close().unwrap();
    }
}
