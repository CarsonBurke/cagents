//! Exact Retry is held locally until native setters and fresh effective reads
//! agree. Generic setter ACKs never supply original-setting evidence.
//! The product capability stays disabled until matched native qualification.
use super::*;
use crate::chat::annotations::{OriginalSettings, OriginalUpdate, Setting};
use crate::chat::protocol::original_proof;
use std::time::{Duration, Instant};

#[derive(Default)]
pub(super) struct RetryState {
    pending: Option<Pending>,
    deferred: Option<(u64, Step)>,
}
struct Pending {
    id: String,
    session: String,
    generation: u64,
    revision: u64,
    deadline: Instant,
    request: String,
    phase: Phase,
    prompt: Command,
    names: Vec<Option<PathBuf>>,
    settings: OriginalSettings,
    applied: Option<Value>,
}
#[derive(Clone, Copy)]
enum Phase { Model, Reasoning, Fast, Permission, Applied, Policy }

impl Claude {
    pub(super) fn begin_original_retry(&mut self, settings: &OriginalSettings, prompt: Command) -> Result<Step, String> {
        if !self.ready || self.busy || self.history_only || self.retry.pending.is_some()
            || self.retry.deferred.is_some() || !self.controls.is_empty()
            || !self.pending.is_empty() || !self.queued.is_empty() || !self.submissions.is_empty()
            || !self.background_agents.is_empty() || self.originals.waiting.is_some()
            || !matches!(&prompt, Command::Prompt { mode: PromptMode::Send, .. })
        {
            return Err("Exact Retry requires an idle native conversation with no pending work".into());
        }
        if let Command::Prompt { text, attachments, .. } = &prompt {
            if text.len() > MAX_TEXT || attachments.len() > ATTACHMENTS.max_count.unwrap()
                || (text.trim().is_empty() && attachments.is_empty())
            { return Err("The original exceeds Claude native prompt bounds".into()); }
        }
        let session = self.original_retry_session().filter(|session| Some(*session) == self.session_id.as_deref())
            .ok_or("Retry requires the actual confirmed native session")?.to_owned();
        let (Setting::Value(model), Setting::Value(reasoning), Setting::Value(_), Setting::Value(permission)) =
            (&settings.model, &settings.reasoning, &settings.fast, &settings.permission)
        else { return Err("Claude Retry requires four concrete original settings".into()); };
        if model.is_empty() || model.len() > 512 || reasoning.is_empty() || reasoning.len() > 64
            || !MODES.contains(&permission.as_str())
        { return Err("The original Claude settings are not restorable".into()); }
        let id = self.originals.staged.take().ok_or("Retry has no captured command identity")?;
        let names = std::mem::take(&mut self.attachment_names);
        self.original_settings_changed();
        let step = match self.control(json!({"subtype":"set_model","model":model})) {
            Ok(step) => step,
            Err(error) => { self.originals.staged = Some(id); self.attachment_names = names; return Err(error); }
        };
        self.retry.pending = Some(Pending {
            id, session, generation:self.attention_generation, revision:self.originals.revision,
            deadline:Instant::now()+Duration::from_secs(20), request:step.writes[0]["request_id"].as_str().unwrap().into(),
            phase:Phase::Model, prompt, names, settings:settings.clone(), applied:None,
        });
        Ok(step)
    }
    pub(super) fn cancel_original_retry(&mut self, reason: &str) -> Option<Step> {
        let pending = self.retry.pending.take()?;
        self.controls.remove(&pending.request);
        self.originals.verified_retry = None;
        self.originals.updates.push(OriginalUpdate::Rejected { command_id:pending.id });
        // No prompt was submitted and no native turn began. Settle only the
        // local optimistic busy indicator; never manufacture an old turn fate.
        let mut step = Step::default();
        if self.attention_generation == pending.generation && !self.busy {
            self.status(false, reason, &mut step);
        }
        step.events.push(Event::Error { message:reason.into(), fatal:false });
        Some(step)
    }
    pub(super) fn merge_retry_abort(&mut self, result:Result<Step,String>, aborted:Option<Step>) -> Result<Step,String> {
        match (result, aborted) {
            (Ok(mut step), Some(aborted)) => {
                step.events.splice(0..0, aborted.events);
                Ok(step)
            }
            (Err(error), Some(aborted)) => {
                // Even a refused newer command must settle the prior held
                // attempt. Maintenance publishes this without pretending the
                // refused command was admitted.
                self.retry.deferred = Some((self.attention_generation, aborted));
                Err(error)
            }
            (result, None) => result,
        }
    }
    pub(super) fn retry_deadline(&self) -> Option<Instant> {
        if self.retry.deferred.is_some() { Some(Instant::now()) }
        else { self.retry.pending.as_ref().map(|pending|pending.deadline) }
    }
    pub(super) fn retry_maintenance(&mut self) -> Step {
        if let Some((generation, step)) = self.retry.deferred.take() {
            // A late settlement belongs only to its held local attempt.
            // It cannot clear busy/error state of a newly observed native turn.
            return if generation == self.attention_generation && !self.busy { step } else { Step::default() };
        }
        if self.retry.pending.as_ref().is_some_and(|pending|Instant::now() >= pending.deadline) {
            return self.cancel_original_retry("Exact Retry timed out before dispatch; no prompt was sent").unwrap();
        }
        Step::default()
    }
    pub(super) fn retry_observe_frame(&mut self, frame:&Value) -> Step {
        if self.retry.pending.is_none() { return Step::default(); }
        let changed = match frame["type"].as_str() {
            Some("keep_alive" | "prompt_suggestion" | "rate_limit_event" | "control_response") => false,
            Some("system") => frame["subtype"] != "session_state_changed" || frame["state"] != "idle",
            _ => true,
        };
        if changed || frame.get("permissionMode").is_some() || frame.get("fast_mode_state").is_some() {
            self.cancel_original_retry("Native conversation changed during Retry restoration; no prompt was sent").unwrap()
        } else { Step::default() }
    }
    pub(super) fn retry_control_response(&mut self, frame:&Value) -> Result<Option<Step>,String> {
        if frame["type"] != "control_response" || self.retry.pending.as_ref().is_none_or(|pending|
            frame["response"]["request_id"].as_str() != Some(pending.request.as_str()))
        { return Ok(None); }
        let pending = self.retry.pending.as_ref().unwrap();
        if Instant::now() >= pending.deadline || self.busy || self.session_id.as_deref() != Some(pending.session.as_str())
            || self.original_retry_session() != Some(pending.session.as_str())
            || self.attention_generation != pending.generation || self.originals.revision != pending.revision
            || !self.pending.is_empty() || !self.queued.is_empty() || !self.submissions.is_empty()
            || !self.background_agents.is_empty()
        { return Ok(self.cancel_original_retry("Retry dispatch guard changed; no prompt was sent")); }
        if frame["response"]["subtype"] != "success" {
            return Ok(self.cancel_original_retry("Claude refused exact settings restoration; no prompt was sent"));
        }
        self.controls.remove(&pending.request.clone());
        let pending = self.retry.pending.as_mut().unwrap();
        let next = match pending.phase {
            Phase::Model => { pending.phase=Phase::Reasoning; json!({"subtype":"apply_flag_settings","settings":{"effortLevel":match &pending.settings.reasoning {Setting::Value(value)=>value,_=>unreachable!()}}}) }
            Phase::Reasoning => { pending.phase=Phase::Fast; json!({"subtype":"apply_flag_settings","settings":{"fastMode":match &pending.settings.fast {Setting::Value(value)=>*value,_=>unreachable!()}}}) }
            Phase::Fast => { pending.phase=Phase::Permission; json!({"subtype":"set_permission_mode","mode":match &pending.settings.permission {Setting::Value(value)=>if value=="manual" {"default"}else{value.as_str()},_=>unreachable!()}}) }
            Phase::Permission => { pending.phase=Phase::Applied; json!({"subtype":"get_settings"}) }
            Phase::Applied => {
                let response=&frame["response"]["response"];
                let model_effort=original_proof::claude_applied_model_effort(response);
                let valid=model_effort.as_ref().is_ok_and(|applied|
                    pending.settings.model==Setting::Value(applied.model.clone())
                    && pending.settings.reasoning==Setting::Value(applied.effort.clone()));
                if !valid { return Ok(self.cancel_original_retry("Claude effective model or effort differs from the original; no prompt was sent")); }
                // Retain only the small confirmed fields, never the arbitrary
                // settings response or credentials/permission tree.
                let applied=model_effort.unwrap();
                pending.applied=Some(json!({"applied":{"model":applied.model,"effort":applied.effort}}));
                pending.phase=Phase::Policy;
                json!({"subtype":"initialize","forwardSubagentText":true,"supportedDialogKinds":[],"promptSuggestions":false})
            }
            Phase::Policy => {
                let response=&frame["response"]["response"];
                let effective=original_proof::claude_applied_settings(
                    pending.applied.as_ref().unwrap(),
                    response["current_permission_mode"].as_str().unwrap_or(""),
                    response["fast_mode_state"].as_str().unwrap_or(""));
                if response["session_state"] != "idle" || effective.as_ref().ok() != Some(&pending.settings) {
                    return Ok(self.cancel_original_retry("Claude did not confirm idle state and exact original policy; no prompt was sent"));
                }
                let pending=self.retry.pending.take().unwrap();
                self.original_connection_policy(response);
                let mut metadata = Step::default();
                if let Setting::Value(model) = &pending.settings.model { self.selected = Some(model.clone()); }
                if let Setting::Value(reasoning) = &pending.settings.reasoning { self.reasoning = Some(reasoning.clone()); }
                self.metadata(&Value::Null, &mut metadata);
                self.fast_mode_metadata(response, &mut metadata);
                if let Setting::Value(mode) = &pending.settings.permission {
                    metadata.events.push(Event::PermissionMode(Some(mode.clone())));
                }
                self.originals.staged=Some(pending.id.clone());
                self.originals.verified_retry=Some((pending.id.clone(),pending.settings));
                self.attachment_names=pending.names;
                match self.command(pending.prompt) {
                    Ok(mut dispatched) => {
                        metadata.writes.append(&mut dispatched.writes);
                        metadata.events.append(&mut dispatched.events);
                    }
                    Err(_) => {
                        self.originals.staged=None;
                        self.originals.verified_retry=None;
                        self.attachment_names.clear();
                        self.submissions.remove(&pending.id);
                        self.queued.retain(|message|message.id!=pending.id);
                        self.turn_inputs.discard_undispatched(&pending.id);
                        self.originals.updates.push(OriginalUpdate::Rejected {command_id:pending.id});
                        self.status(false, "Exact Retry input could not be encoded; no prompt was sent", &mut metadata);
                        metadata.events.push(Event::Error {message:"Exact Retry input could not be encoded; no prompt was sent".into(),fatal:false});
                    }
                }
                return Ok(Some(metadata));
            }
        };
        match self.control(next) {
            Ok(step) => {
                self.retry.pending.as_mut().unwrap().request=step.writes[0]["request_id"].as_str().unwrap().into();
                Ok(Some(step))
            }
            Err(_) => Ok(self.cancel_original_retry("Claude Retry control admission failed; no prompt was sent")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const SESSION:&str="00000000-0000-4000-8000-000000000001";
    fn settings() -> OriginalSettings {
        OriginalSettings {model:Setting::Value("original-model".into()),reasoning:Setting::Value("low".into()),fast:Setting::Value(false),permission:Setting::Value("manual".into())}
    }
    fn ready_adapter() -> Claude {
        let mut adapter=Claude {ready:true,async_input:true,..Claude::default()};
        adapter.receive(json!({"type":"system","subtype":"init","session_id":SESSION,"model":"different-model","effort":"high","permissionMode":"bypassPermissions","fast_mode_state":"on"})).unwrap();
        adapter
    }
    fn prompt(mode:PromptMode) -> Command {
        Command::Prompt {text:"exact original".into(),attachments:vec![],mode}
    }
    fn begin(adapter:&mut Claude) -> Step {
        adapter.original_command(Some("exact-retry-command".into()));
        adapter.retry_original(&settings(),prompt(PromptMode::Send)).unwrap()
    }
    fn response(step:&Step, data:Value) -> Value {
        assert_eq!(step.writes.len(),1);
        json!({"type":"control_response","response":{"request_id":step.writes[0]["request_id"],"subtype":"success","response":data}})
    }
    fn advance_to_policy(adapter:&mut Claude, mut step:Step) -> Step {
        for subtype in ["set_model","apply_flag_settings","apply_flag_settings","set_permission_mode"] {
            assert_eq!(step.writes[0]["request"]["subtype"],subtype);
            assert!(step.writes.iter().all(|frame|frame["type"]!="user"));
            step=adapter.receive(response(&step,json!({}))).unwrap();
        }
        assert_eq!(step.writes[0]["request"]["subtype"],"get_settings");
        adapter.receive(response(&step,json!({"applied":{"model":"original-model","effort":"low"}}))).unwrap()
    }
    fn policy() -> Value {
        json!({"current_permission_mode":"default","fast_mode_state":"off","session_state":"idle"})
    }
    #[test]
    fn retry_restores_then_proves_effective_settings_before_exact_single_dispatch() {
        let mut adapter=ready_adapter();let first=begin(&mut adapter);
        assert_eq!(first.writes[0]["request"]["model"],"original-model");
        let second=adapter.receive(response(&first,json!({}))).unwrap();
        assert_eq!(second.writes[0]["request"]["settings"]["effortLevel"],"low");
        let third=adapter.receive(response(&second,json!({}))).unwrap();
        assert_eq!(third.writes[0]["request"]["settings"]["fastMode"],false);
        let fourth=adapter.receive(response(&third,json!({}))).unwrap();
        assert_eq!(fourth.writes[0]["request"]["mode"],"default");
        let applied=adapter.receive(response(&fourth,json!({}))).unwrap();
        let final_read=adapter.receive(response(&applied,json!({"applied":{"model":"original-model","effort":"low"}}))).unwrap();
        assert_eq!(final_read.writes[0]["request"]["subtype"],"initialize");
        let dispatched=adapter.receive(response(&final_read,policy())).unwrap();
        assert_eq!(dispatched.writes.len(),1);
        assert_eq!(dispatched.writes[0]["type"],"user");
        assert_eq!(dispatched.writes[0]["uuid"],"exact-retry-command");
        assert_eq!(dispatched.writes[0]["session_id"],SESSION);
        assert!(adapter.originals.waiting.is_none(),"fresh verified dispatch must not fall back into optional metadata timeout");
        assert!(!adapter.capabilities().original_retry,"native qualification has not enabled product capability");
        assert!(adapter.take_original_updates().is_empty(),"dispatch is not provider acceptance");
        adapter.receive(json!({"type":"user","uuid":"exact-retry-command","session_id":SESSION,"message":{"id":"native-echo","role":"user","content":"provider echo"}})).unwrap();
        adapter.receive(json!({"type":"command_lifecycle","session_id":SESSION,"command_uuid":"exact-retry-command","state":"started"})).unwrap();
        let updates=adapter.take_original_updates();
        assert!(updates.iter().any(|update|matches!(update,OriginalUpdate::Settings {settings:actual,..} if actual==&settings())));
        assert!(updates.iter().any(|update|matches!(update,OriginalUpdate::Accepted {command_id,..} if command_id=="exact-retry-command")));
    }
    #[test]
    fn retry_requires_actual_session_concrete_policy_and_idle_admission() {
        let mut adapter=Claude {ready:true,session_id:Some(SESSION.into()),..Claude::default()};
        adapter.original_command(Some("capture".into()));
        assert!(adapter.retry_original(&settings(),prompt(PromptMode::Send)).is_err());
        for invalid in [Setting::Unknown,Setting::Default,Setting::Unsupported] {
            let mut adapter=ready_adapter();let mut unknown=settings();unknown.fast=invalid;
            adapter.original_command(Some("capture".into()));
            assert!(adapter.retry_original(&unknown,prompt(PromptMode::Send)).is_err());
            assert!(adapter.controls.is_empty());
        }
    }
    #[test]
    fn retry_wrong_or_missing_effective_policy_never_dispatches() {
        for field in ["current_permission_mode","fast_mode_state","session_state"] {
            for missing in [false,true] {
                let mut adapter=ready_adapter();let first=begin(&mut adapter);let final_read=advance_to_policy(&mut adapter,first);
                let mut report=policy();if missing {report.as_object_mut().unwrap().remove(field);}else{report[field]=json!("different");}
                let refused=adapter.receive(response(&final_read,report)).unwrap();
                assert!(refused.writes.is_empty());assert!(adapter.retry.pending.is_none());assert!(adapter.controls.is_empty());
                assert!(adapter.take_original_updates().iter().any(|update|matches!(update,OriginalUpdate::Rejected{command_id} if command_id=="exact-retry-command")));
            }
        }
    }
    #[test]
    fn retry_unknown_or_different_applied_settings_never_dispatches() {
        for applied in [json!({}),json!({"model":"requested-only","effort":"low"}),json!({"model":"original-model","effort":null})] {
            let mut adapter=ready_adapter();let mut step=begin(&mut adapter);
            for _ in 0..4 {step=adapter.receive(response(&step,json!({}))).unwrap();}
            let refused=adapter.receive(response(&step,json!({"applied":applied}))).unwrap();
            assert!(refused.writes.is_empty());assert!(adapter.retry.pending.is_none());
        }
    }
    #[test]
    fn retry_timeout_and_refusal_have_no_optional_send_fallback_or_late_reply() {
        for timeout in [false,true] {
            let mut adapter=ready_adapter();let first=begin(&mut adapter);let late=response(&first,json!({}));
            let refused=if timeout {
                adapter.retry.pending.as_mut().unwrap().deadline=Instant::now();adapter.maintenance().unwrap()
            }else{
                let mut refused=late.clone();refused["response"]["subtype"]=json!("error");adapter.receive(refused).unwrap()
            };
            assert!(refused.writes.is_empty());assert!(!adapter.busy);
            assert!(adapter.receive(late).unwrap().writes.is_empty());assert!(adapter.maintenance().unwrap().writes.is_empty());
            assert!(adapter.take_original_updates().iter().any(|update|matches!(update,OriginalUpdate::Rejected{..})));
        }
    }
    #[test]
    fn retry_interrupt_close_and_newer_native_work_retire_exact_unsent_intent() {
        for action in ["interrupt","close","native"] {
            let mut adapter=ready_adapter();let first=begin(&mut adapter);let late=response(&first,json!({}));
            let stopped=match action {
                "interrupt"=>adapter.command(Command::Interrupt).unwrap(),
                "close"=>adapter.discard_undispatched().unwrap().unwrap(),
                _=>adapter.receive(json!({"type":"system","subtype":"session_state_changed","session_id":SESSION,"state":"working"})).unwrap(),
            };
            assert!(stopped.writes.is_empty());assert!(adapter.retry.pending.is_none());
            assert!(adapter.receive(late).unwrap().writes.is_empty());
            assert!(adapter.take_original_updates().iter().any(|update|matches!(update,OriginalUpdate::Rejected{command_id} if command_id=="exact-retry-command")));
            assert_eq!(adapter.busy,action=="native");
        }
    }
    #[test]
    fn retry_new_queued_input_remains_distinct_and_refused_actions_settle_held_attempt() {
        let mut adapter=ready_adapter();let first=begin(&mut adapter);let late=response(&first,json!({}));
        let queued=adapter.command(prompt(PromptMode::Queue)).unwrap();
        assert_eq!(queued.writes.len(),1);assert_eq!(queued.writes[0]["type"],"user");
        assert_ne!(queued.writes[0]["uuid"],"exact-retry-command");
        assert!(adapter.receive(late).unwrap().writes.is_empty());
        let mut adapter=ready_adapter();begin(&mut adapter);
        assert!(adapter.command(Command::SetModel{model:"unadvertised".into(),reasoning:None}).is_err());
        assert!(adapter.maintenance_deadline().is_some());
        let settled=adapter.maintenance().unwrap();assert!(settled.writes.is_empty());
        assert!(settled.events.iter().any(|event|matches!(event,Event::Status{busy:false,..})));
    }
}
