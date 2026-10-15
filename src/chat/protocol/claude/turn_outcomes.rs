//! Accepted-input ownership is independent of optional original settings.
//! Lifecycle completion is NOT success: the native engine also uses it for
//! error_max_turns. No terminal cause or foreground tool ownership is guessed.
use super::*;
use crate::chat::{protocol::input_proof::ClaudeProofs,turn_outcomes::{TurnIdentity,TurnIdentityKind,TurnUpdate}};

const PENDING:usize=64;
const RECENT:usize=128;
const ALIAS_BYTES:usize=64*1024;
#[derive(Default)]
pub(super) struct Inputs {
    proofs:ClaudeProofs,
    pending:VecDeque<(String,Vec<String>)>,
    recent:VecDeque<(String,TurnIdentity)>,
    confirmed:Option<String>,
}
fn user_aliases(events:&[Event])->Vec<String>{
    events.iter().filter_map(|event|match event{Event::Item(item) if item.role==Role::User=>Some(item.id.clone()),_=>None}).collect()
}
fn emit_bind(identity:&TurnIdentity,ids:Vec<String>,step:&mut Step){
    use crate::chat::turn_outcomes::{MAX_TURN_ITEM_IDS,MAX_TURN_UPDATE_BYTES,MAX_TURN_ID_BYTES};
    let base=64+identity.session_id.len()+identity.id.len();
    let mut batch=Vec::new();let mut bytes=base;
    for id in ids{
        if id.is_empty() || id.len()>MAX_TURN_ID_BYTES || batch.contains(&id){continue;}
        let size=id.len()+8;
        if !batch.is_empty() && (batch.len()==MAX_TURN_ITEM_IDS || bytes.saturating_add(size)>MAX_TURN_UPDATE_BYTES){
            let update=TurnUpdate::Bind{identity:identity.clone(),item_ids:std::mem::take(&mut batch)};
            if update.validate().is_ok(){step.events.push(Event::TurnOutcome(update));}bytes=base;
        }
        if base.saturating_add(size)>MAX_TURN_UPDATE_BYTES{continue;}
        bytes+=size;batch.push(id);
    }
    if !batch.is_empty(){let update=TurnUpdate::Bind{identity:identity.clone(),item_ids:batch};if update.validate().is_ok(){step.events.push(Event::TurnOutcome(update));}}
}
impl Inputs {
    pub(super) fn submit(&mut self,uuid:&str,events:&[Event]) {
        while self.pending.len()>=PENDING {
            if let Some((old,_))=self.pending.pop_front(){self.proofs.cancel(&old);}
        }
        let aliases=user_aliases(events);
        let retained:usize=self.pending.iter().flat_map(|(_,ids)|ids).map(String::len).sum();
        let bytes:usize=aliases.iter().map(String::len).sum();
        if aliases.len()>64 || bytes>ALIAS_BYTES.saturating_sub(retained){return;}
        if self.proofs.submit(uuid,uuid,self.confirmed.as_deref()).is_ok(){self.pending.push_back((uuid.into(),aliases));}
    }
    /// Only local proof of never writing, never a generic interruption result.
    pub(super) fn discard_undispatched(&mut self,uuid:&str){
        self.proofs.cancel(uuid);self.pending.retain(|(id,_)|id!=uuid);
    }
    pub(super) fn observe(&mut self,frame:&Value,step:&mut Step,native_session:Option<&str>) {
        if frame["type"]=="conversation_reset"{*self=Self::default();return;}
        if frame["type"]=="prompt_suggestion" || !frame["parent_tool_use_id"].is_null(){return;}
        // Only the main decoder can establish native session authority. An
        // optional suggestion or unrelated frame cannot seed this context.
        if let Some(session)=native_session {
            if !crate::chat::protocol::input_proof::valid_id(session){return;}
            if self.confirmed.as_deref().is_some_and(|known|known!=session){return;}
            self.confirmed=Some(session.into());
        }
        let aliases=if frame["type"]=="user"{user_aliases(&step.events)}else{vec![]};
        if let Some(proof)=self.proofs.observe(frame,&aliases,self.confirmed.as_deref()) {
            let identity=TurnIdentity{session_id:proof.session_id,kind:TurnIdentityKind::AcceptedInput,id:proof.user_id};
            if identity.validate().is_err(){return;}
            let mut ids=proof.aliases;
            if let Some(index)=self.pending.iter().position(|(id,_)|id==&proof.command_id) {
                let (_,local)=self.pending.remove(index).unwrap();
                for id in local{if !ids.contains(&id){ids.push(id);}}
            }
            // The canonical received user UUID is also exact identity, even if
            // its native display rows are deferred or outside this window.
            if !ids.contains(&identity.id){ids.push(identity.id.clone());}
            emit_bind(&identity,ids,step);
            self.recent.push_back((proof.command_id,identity));
            while self.recent.len()>RECENT{self.recent.pop_front();}
        } else if frame["type"]=="user" && frame["isSynthetic"].as_bool()!=Some(true) && frame["message"]["role"]=="user" && !aliases.is_empty()
            && let Some((_,identity))=self.recent.iter().find(|(uuid,identity)|frame["uuid"].as_str()==Some(uuid.as_str()) && frame["session_id"].as_str()==Some(identity.session_id.as_str())) {
            // A later real replay can expose additional native parser aliases.
            emit_bind(identity,aliases,step);
        }
        if frame["type"]=="command_lifecycle" && matches!(frame["state"].as_str(),Some("refused"|"discarded")) && let Some(uuid)=frame["command_uuid"].as_str(){self.pending.retain(|(id,_)|id!=uuid);}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const SESSION:&str="00000000-0000-4000-8000-000000000001";
    fn adapter()->Claude{Claude{ready:true,async_input:true,..Claude::default()}}
    fn prompt(mode:PromptMode)->Command{Command::Prompt{text:"same consecutive prompt".into(),attachments:vec![],mode}}
    fn echo(uuid:&str)->Value{json!({"type":"user","uuid":uuid,"session_id":SESSION,"parent_tool_use_id":null,"message":{"role":"user","id":format!("native-{uuid}"),"content":[{"type":"text","text":"provider echo"}]}})}
    fn fate(uuid:&str,state:&str)->Value{json!({"type":"command_lifecycle","command_uuid":uuid,"state":state,"session_id":SESSION})}
    fn bindings(step:&Step)->Vec<&TurnIdentity>{step.events.iter().filter_map(|event|match event{Event::TurnOutcome(TurnUpdate::Bind{identity,..})=>Some(identity),_=>None}).collect()}
    #[test]
    fn actual_decoder_binds_exact_accepted_input_in_either_receipt_order_without_original_settings(){
        for echo_first in [true,false]{
            let mut adapter=adapter();let sent=adapter.command(prompt(PromptMode::Send)).unwrap();let uuid=sent.writes[0]["uuid"].as_str().unwrap();assert!(bindings(&sent).is_empty());
            let first=adapter.receive(if echo_first{echo(uuid)}else{fate(uuid,"started")}).unwrap();assert!(bindings(&first).is_empty());
            let accepted=adapter.receive(if echo_first{fate(uuid,"started")}else{echo(uuid)}).unwrap();let ids=bindings(&accepted);assert_eq!(ids.len(),1);assert_eq!(ids[0].kind,TurnIdentityKind::AcceptedInput);assert_eq!(ids[0].id,uuid);assert_eq!(ids[0].session_id,SESSION);
        }
    }
    #[test]
    fn suggestion_before_first_session_cannot_poison_accepted_input_authority(){
        let mut adapter=adapter();
        adapter.receive(json!({"type":"prompt_suggestion","session_id":"wrong-prediction-session","suggestion":"optional"})).unwrap();
        assert!(adapter.session_id.is_none());
        assert!(adapter.turn_inputs.confirmed.is_none());
        let sent=adapter.command(prompt(PromptMode::Send)).unwrap();let uuid=sent.writes[0]["uuid"].as_str().unwrap();
        assert!(bindings(&adapter.receive(echo(uuid)).unwrap()).is_empty());
        let step=adapter.receive(fate(uuid,"started")).unwrap();
        assert_eq!(bindings(&step)[0].session_id,SESSION);
    }
    #[test]
    fn suggestion_cannot_supply_missing_session_authority_to_proof_frames(){
        let mut adapter=adapter();
        adapter.receive(json!({"type":"prompt_suggestion","session_id":SESSION,"suggestion":"optional"})).unwrap();
        let sent=adapter.command(prompt(PromptMode::Send)).unwrap();let uuid=sent.writes[0]["uuid"].as_str().unwrap();
        let mut user=echo(uuid);user.as_object_mut().unwrap().remove("session_id");
        let mut started=fate(uuid,"started");started.as_object_mut().unwrap().remove("session_id");
        assert!(bindings(&adapter.receive(user).unwrap()).is_empty());
        assert!(bindings(&adapter.receive(started).unwrap()).is_empty());
        assert!(adapter.session_id.is_none());
        assert!(adapter.turn_inputs.confirmed.is_none());
    }
    #[test]
    fn launch_session_constraint_does_not_confirm_missing_native_identity(){
        let mut adapter=adapter();adapter.session_id=Some(SESSION.into());
        let sent=adapter.command(prompt(PromptMode::Send)).unwrap();let uuid=sent.writes[0]["uuid"].as_str().unwrap();
        let mut user=echo(uuid);user.as_object_mut().unwrap().remove("session_id");
        let mut started=fate(uuid,"started");started.as_object_mut().unwrap().remove("session_id");
        assert!(bindings(&adapter.receive(user).unwrap()).is_empty());
        assert!(bindings(&adapter.receive(started).unwrap()).is_empty());
        assert!(adapter.turn_inputs.confirmed.is_none());
    }
    #[test]
    fn queued_identical_inputs_keep_distinct_accepted_identity(){
        let mut adapter=adapter();let a=adapter.command(prompt(PromptMode::Queue)).unwrap();let b=adapter.command(prompt(PromptMode::Queue)).unwrap();let a=a.writes[0]["uuid"].as_str().unwrap();let b=b.writes[0]["uuid"].as_str().unwrap();assert_ne!(a,b);
        assert!(bindings(&adapter.receive(echo(a)).unwrap()).is_empty());assert!(bindings(&adapter.receive(echo(b)).unwrap()).is_empty());
        let first=adapter.receive(fate(a,"started")).unwrap();let second=adapter.receive(fate(b,"started")).unwrap();assert_eq!(bindings(&first)[0].id,a);assert_eq!(bindings(&second)[0].id,b);
    }
    #[test]
    fn completed_or_cancelled_lifecycle_never_guesses_success_or_cancellation_cause(){
        for terminal in ["completed","cancelled"]{
            let mut adapter=adapter();let sent=adapter.command(prompt(PromptMode::Send)).unwrap();let uuid=sent.writes[0]["uuid"].as_str().unwrap();adapter.receive(fate(uuid,"started")).unwrap();adapter.receive(echo(uuid)).unwrap();
            let step=adapter.receive(fate(uuid,terminal)).unwrap();assert!(step.events.iter().all(|event|!matches!(event,Event::TurnOutcome(TurnUpdate::Finish{..}))));
        }
    }
    #[test]
    fn never_dispatched_input_and_local_settlement_cannot_bind_prior_input(){
        let mut inputs=Inputs::default();inputs.submit("held",&[]);inputs.discard_undispatched("held");let mut step=Step{events:vec![Event::TurnSettled,Event::TurnFinished{key:None,outcome:crate::notifications::Kind::Cancelled}],..Step::default()};inputs.observe(&fate("held","started"),&mut step,None);inputs.observe(&echo("held"),&mut step,None);assert!(bindings(&step).is_empty());
    }
}
