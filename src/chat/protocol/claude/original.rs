//! Original capture uses a fresh provider settings read immediately before
//! direct input dispatch. A bounded timeout preserves ordinary sending when
//! this optional metadata operation is unsupported or unresponsive.
use super::*;
use crate::chat::annotations::OriginalUpdate;
use crate::chat::protocol::original_proof::{self,AcceptedPromptProof,ClaudeProofs};
use std::collections::BTreeMap;
use std::time::{Duration,Instant};

pub(super) struct Waiting {
    pub deadline:Instant,
    request:String,
    command:String,
    revision:u64,
    frames:Vec<Value>,
    bytes:usize,
}
#[derive(Default)]
pub(super) struct Originals {
    pub staged:Option<String>,
    pub revision:u64,
    pub updates:Vec<OriginalUpdate>,
    pub waiting:Option<Waiting>,
    proofs:ClaudeProofs,
    applied:BTreeMap<String,(Value,u64)>,
    accepted:BTreeMap<String,AcceptedPromptProof>,
    init:Option<Value>,
    confirmed_session:Option<String>,
    latest_started:Option<String>,
}
impl Claude {
    pub(super) fn original_frame_budget(&self,frame:&Value)->Result<(),String>{
        if let Some(waiting)=&self.originals.waiting && encoded_len(frame)?>MAX_PAYLOAD.saturating_sub(waiting.bytes){return Err("Claude pending settings and input exceed the native input budget".into());}
        Ok(())
    }
    pub(super) fn original_dispatch(&mut self,frame:Value,command:Option<String>)->Result<Vec<Value>,String>{
        if let Some(waiting)=&mut self.originals.waiting {
            waiting.bytes=waiting.bytes.saturating_add(encoded_len(&frame)?);waiting.frames.push(frame);return Ok(vec![]);
        }
        let Some(command)=command else{return Ok(vec![frame]);};
        if self.originals.proofs.submit(&command,&command,self.originals.confirmed_session.as_deref()).is_err(){return Ok(vec![frame]);}
        // Read optional metadata first. Failure to allocate this optional RPC
        // cannot prevent dispatch of the user's already validated prompt.
        let read=match self.control(json!({"subtype":"get_settings"})) {Ok(read)=>read,Err(_)=>return Ok(vec![frame])};
        let request=read.writes[0]["request_id"].as_str().ok_or("Claude settings request has no ID")?.to_owned();
        self.originals.waiting=Some(Waiting{deadline:Instant::now()+Duration::from_secs(1),request,command,revision:self.originals.revision,bytes:encoded_len(&frame)?,frames:vec![frame]});
        Ok(read.writes)
    }
    pub(super) fn original_flush(&mut self)->Vec<Value>{
        if let Some(waiting)=self.originals.waiting.take(){self.controls.remove(&waiting.request);waiting.frames}else{vec![]}
    }
    pub(super) fn original_maintenance(&mut self)->Step{
        if self.originals.waiting.as_ref().is_some_and(|waiting|Instant::now()>=waiting.deadline){
            let waiting=self.originals.waiting.take().unwrap();self.controls.remove(&waiting.request);
            return Step{writes:waiting.frames,..Step::default()};
        }
        Step::default()
    }
    pub(super) fn original_control_response(&mut self,frame:&Value)->Option<Step>{
        if frame["type"]!="control_response" || self.originals.waiting.as_ref().is_none_or(|waiting|frame["response"]["request_id"].as_str()!=Some(waiting.request.as_str())){return None;}
        let waiting=self.originals.waiting.take().unwrap();self.controls.remove(&waiting.request);
        if frame["response"]["subtype"]=="success" && waiting.revision==self.originals.revision && self.originals.applied.len()<64
            && let Ok(applied)=original_proof::claude_applied_model_effort(&frame["response"]["response"]){
            self.originals.applied.insert(waiting.command,(json!({"applied":{"model":applied.model,"effort":applied.effort}}),waiting.revision));
        }
        Some(Step{writes:waiting.frames,..Step::default()})
    }
    pub(super) fn original_observe(&mut self,frame:&Value,step:&Step){
        if frame["type"]=="conversation_reset" {
            if let Some(waiting)=self.originals.waiting.take(){self.controls.remove(&waiting.request);self.originals.updates.push(OriginalUpdate::Rejected{command_id:waiting.command});}
            self.originals.proofs=ClaudeProofs::default();self.originals.applied.clear();self.originals.accepted.clear();self.originals.init=None;self.originals.confirmed_session=None;self.originals.latest_started=None;self.originals.revision=self.originals.revision.wrapping_add(1);return;
        }
        // The enclosing receive path already validated this actual provider
        // identity against the connection. Launch --session-id is not evidence.
        if let Some(session)=frame["session_id"].as_str(){self.originals.confirmed_session=Some(session.to_owned());}
        if frame["type"]=="system" && frame["subtype"]=="init" {
            let mut init=serde_json::Map::new();
            for key in ["type","subtype","session_id","model","effort","permissionMode","fast_mode_state"]{if let Some(value)=frame.get(key){init.insert(key.into(),value.clone());}}
            self.originals.init=Some(Value::Object(init));
        }else if let Some(init)=self.originals.init.as_mut(){
            // Autonomous provider changes invalidate a previously sampled
            // setting. They must never silently become this prompt's settings.
            for key in ["permissionMode","fast_mode_state"]{
                if let Some(value)=frame.get(key) && init.get(key)!=Some(value){self.originals.revision=self.originals.revision.wrapping_add(1);init[key]=value.clone();}
            }
        }
        if frame["type"]=="command_lifecycle" && let Some(command)=frame["command_uuid"].as_str(){
            match frame["state"].as_str(){
                Some("started")=>{self.originals.latest_started=Some(command.to_owned());},
                Some("refused"|"cancelled"|"discarded")=>{
                    // These exact terminal provider decisions prove that the
                    // submitted command will not execute. Unlike disconnect,
                    // they permit retiring its captured private bytes.
                    self.originals.applied.remove(command);self.originals.accepted.remove(command);
                    self.originals.updates.push(OriginalUpdate::Rejected{command_id:command.to_owned()});
                },_=>{},
            }
        }
        if let Some(session)=self.originals.confirmed_session.as_ref() && let Some(user)=self.originals.latest_started.take(){
            self.originals.updates.push(OriginalUpdate::Latest{session_id:session.clone(),user_id:user});
        }
        let aliases=if frame["type"]=="user"{step.events.iter().filter_map(|event|match event{Event::Item(item) if item.role==Role::User=>Some(item.id.clone()),_=>None}).collect::<Vec<_>>()}else{vec![]};
        if let Some(proof)=self.originals.proofs.observe(frame,&aliases,self.originals.confirmed_session.as_deref()){
            if self.originals.accepted.len()<64{self.originals.accepted.insert(proof.command_id.clone(),proof);}
        }
        let Some(init)=&self.originals.init else{return;};
        let ready=self.originals.accepted.iter().filter_map(|(id,proof)|{
            let (applied,revision)=self.originals.applied.get(id)?;
            if *revision!=self.originals.revision{return None;}
            let settings=original_proof::claude_reported_settings(applied,init,&proof.session_id).ok()?;
            Some((id.clone(),settings))
        }).collect::<Vec<_>>();
        for (id,settings) in ready{
            let proof=self.originals.accepted.remove(&id).unwrap();self.originals.applied.remove(&id);
            self.originals.updates.push(OriginalUpdate::Settings{command_id:id.clone(),settings});
            self.originals.updates.push(OriginalUpdate::Accepted{command_id:id,session_id:proof.session_id,user_id:proof.user_id,turn_id:proof.turn_id,aliases:proof.aliases});
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn adapter()->Claude{Claude{ready:true,async_input:true,..Claude::default()}}
    fn prompt(text:&str,mode:PromptMode)->Command{Command::Prompt{text:text.into(),attachments:vec![],mode}}
    fn settings_reply(request:&Value)->Value{json!({"type":"control_response","response":{"request_id":request["request_id"],"subtype":"success","response":{"applied":{"model":"reported-model","effort":"low"}}}})}
    fn init()->Value{json!({"type":"system","subtype":"init","session_id":"00000000-0000-4000-8000-000000000001","model":"reported-model","effort":"low","permissionMode":"default","fast_mode_state":"off"})}
    fn user(command:&str)->Value{json!({"type":"user","uuid":command,"session_id":"00000000-0000-4000-8000-000000000001","parent_tool_use_id":null,"message":{"id":"actual-provider-root","role":"user","content":[{"type":"text","text":"provider-expanded echo"}]}})}
    fn started(command:&str)->Value{json!({"type":"command_lifecycle","command_uuid":command,"state":"started"})}
    #[test]
    fn actual_decoder_first_prompt_joins_dispatch_settings_native_session_user_and_lifecycle(){
        for echo_first in [false,true]{
            let mut adapter=adapter();adapter.original_command(Some("exact-command".into()));
            let read=adapter.command(prompt("original /command",PromptMode::Send)).unwrap();assert_eq!(read.writes[0]["request"]["subtype"],"get_settings");assert!(read.writes.iter().all(|frame|frame["type"]!="user"));
            let dispatched=adapter.receive(settings_reply(&read.writes[0])).unwrap();assert_eq!(dispatched.writes[0]["uuid"],"exact-command");
            adapter.receive(init()).unwrap();assert!(adapter.take_original_updates().is_empty());
            if echo_first{adapter.receive(user("exact-command")).unwrap();adapter.receive(started("exact-command")).unwrap();}else{adapter.receive(started("exact-command")).unwrap();adapter.receive(user("exact-command")).unwrap();}
            let updates=adapter.take_original_updates();assert!(updates.iter().any(|update|matches!(update,OriginalUpdate::Settings{command_id,settings} if command_id=="exact-command"&&settings.model==chat::annotations::Setting::Value("reported-model".into()))));
            assert!(updates.iter().any(|update|matches!(update,OriginalUpdate::Accepted{command_id,user_id,aliases,..} if command_id=="exact-command"&&user_id=="exact-command"&&!aliases.is_empty())));assert!(updates.iter().any(|update|matches!(update,OriginalUpdate::Latest{user_id,..} if user_id=="exact-command")));
        }
    }
    #[test]
    fn mutating_command_flushes_original_input_before_setting_change_and_invalidates_proof(){
        let mut adapter=adapter();adapter.original_command(Some("command".into()));let read=adapter.command(prompt("original",PromptMode::Send)).unwrap();
        let change=adapter.command(Command::SetModel{model:"new-model".into(),reasoning:Some("high".into())}).unwrap();assert_eq!(change.writes[0]["type"],"user");assert_eq!(change.writes[1]["request"]["subtype"],"set_model");
        assert!(adapter.receive(settings_reply(&read.writes[0])).unwrap().writes.is_empty());adapter.receive(init()).unwrap();adapter.receive(started("command")).unwrap();adapter.receive(user("command")).unwrap();assert!(adapter.take_original_updates().iter().all(|update|!matches!(update,OriginalUpdate::Settings{..}|OriginalUpdate::Accepted{..})));
    }
    #[test]
    fn timeout_and_queued_input_preserve_wire_order_without_inventing_settings(){
        let mut adapter=adapter();adapter.original_command(Some("command".into()));let _=adapter.command(prompt("first",PromptMode::Send)).unwrap();
        let queued=adapter.command(prompt("second",PromptMode::Queue)).unwrap();assert!(queued.writes.is_empty());adapter.originals.waiting.as_mut().unwrap().deadline=Instant::now();
        let flushed=adapter.maintenance().unwrap();assert_eq!(flushed.writes.len(),2);assert_eq!(flushed.writes[0]["uuid"],"command");assert_eq!(flushed.writes[0]["message"]["content"][0]["text"],"first");assert_eq!(flushed.writes[1]["message"]["content"][0]["text"],"second");assert!(adapter.maintenance_deadline().is_none());assert!(adapter.maintenance().unwrap().writes.is_empty());
        adapter.receive(init()).unwrap();adapter.receive(started("command")).unwrap();adapter.receive(user("command")).unwrap();assert!(adapter.take_original_updates().iter().all(|update|!matches!(update,OriginalUpdate::Settings{..}|OriginalUpdate::Accepted{..})));
    }
}
