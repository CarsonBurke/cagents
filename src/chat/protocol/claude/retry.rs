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
    #[cfg(test)]
    pub(super) capability: bool,
    #[cfg(test)]
    pub(super) timeout: Option<Duration>,
}
struct Pending {
    id: Option<String>,
    session: String,
    generation: u64,
    revision: u64,
    deadline: Instant,
    request: String,
    phase: Phase,
    prompt: Option<Command>,
    names: Vec<Option<PathBuf>>,
    settings: OriginalSettings,
    applied: Option<Value>,
    cancelled: Option<String>,
}
#[derive(Clone, Copy)]
enum Phase { Model, Reasoning, Fast, Permission, Applied, Policy, Blocked }

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
            id:Some(id), session, generation:self.attention_generation, revision:self.originals.revision,
            deadline:Instant::now()+self.retry_timeout(), request:step.writes[0]["request_id"].as_str().unwrap().into(),
            phase:Phase::Model, prompt:Some(prompt), names, settings:settings.clone(), applied:None, cancelled:None,
        });
        let mut step=step;
        step.events.push(Event::OriginalRetryState {pending:true,diagnostic:None});
        Ok(step)
    }
    pub(super) fn retry_capability(&self) -> bool {
        #[cfg(test)] { return self.retry.capability; }
        #[cfg(not(test))] { false }
    }
    fn retry_timeout(&self) -> Duration {
        #[cfg(test)] if let Some(timeout)=self.retry.timeout { return timeout; }
        Duration::from_secs(20)
    }
    pub(super) fn retry_safe_cleanup(command:&Command) -> bool {
        matches!(command,Command::Interrupt|Command::StopSubagent{..}|Command::RemoveQueued{..}|Command::DismissPermissionDenial{..})
            || matches!(command,Command::Answer{action,..} if matches!(action.as_str(),"deny"|"decline"|"reject"|"cancel"|"abort"))
    }
    pub(super) fn retry_is_pending(&self) -> bool { self.retry.pending.is_some() }
    pub(super) fn cancel_original_retry(&mut self, reason: &str) -> Option<Step> {
        let pending=self.retry.pending.as_mut()?;
        if pending.cancelled.is_none() {
            pending.cancelled=Some(reason.into());
            pending.prompt=None;
            pending.names.clear();
            if let Some(id)=pending.id.take() {
                self.originals.updates.push(OriginalUpdate::Rejected {command_id:id});
            }
        }
        // An already-written setter cannot be undone by dropping its ACK.
        // Keep its correlation, then read effective settings before admitting
        // newer work. This is a local operation, never a provider turn/error.
        Some(Step {events:vec![Event::OriginalRetryState {pending:true,diagnostic:Some(format!("{reason}. Reconciling effective settings before accepting more input"))}],..Step::default()})
    }
    pub(super) fn drop_original_retry(&mut self, reason:&str) -> Option<Step> {
        let mut step=self.cancel_original_retry(reason)?;
        let pending=self.retry.pending.take().unwrap();
        self.controls.remove(&pending.request);
        self.originals.verified_retry=None;
        self.retry.deferred=None;
        step.events=vec![Event::OriginalRetryState {pending:false,diagnostic:Some(reason.into())}];
        Some(step)
    }
    fn block_retry_reconciliation(&mut self, reason:&str) -> Step {
        let mut step=self.cancel_original_retry(reason).unwrap();
        let pending=self.retry.pending.as_mut().unwrap();
        if !matches!(pending.phase,Phase::Model|Phase::Reasoning|Phase::Fast|Phase::Permission) {
            self.controls.remove(&pending.request);
            pending.request.clear();
        }
        pending.phase=Phase::Blocked;
        step.events=vec![Event::OriginalRetryState {pending:true,diagnostic:Some(format!("{reason}. Native settings remain unverified; refresh the conversation before sending more input"))}];
        step
    }
    pub(super) fn refresh_cancelled_retry(&mut self) -> Result<Step,String> {
        if !self.retry.pending.as_ref().is_some_and(|pending|matches!(pending.phase,Phase::Blocked)) {
            return Err("Exact Retry settings reconciliation is still pending".into());
        }
        if !self.retry.pending.as_ref().unwrap().request.is_empty() {
            return Err("A prior native setter has not settled; settings cannot yet be revalidated".into());
        }
        let mut step=self.control(json!({"subtype":"get_settings"}))?;
        let timeout=Duration::from_secs(20);
        let pending=self.retry.pending.as_mut().unwrap();
        pending.phase=Phase::Applied;pending.deadline=Instant::now()+timeout;
        pending.request=step.writes[0]["request_id"].as_str().unwrap().into();
        step.events.push(Event::OriginalRetryState {pending:true,diagnostic:Some("Refreshing effective native settings; no original prompt will be sent".into())});
        Ok(step)
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
        else { self.retry.pending.as_ref().filter(|pending|!matches!(pending.phase,Phase::Blocked)).map(|pending|pending.deadline) }
    }
    pub(super) fn retry_maintenance(&mut self) -> Step {
        if let Some((generation, step)) = self.retry.deferred.take() {
            // A late settlement belongs only to its held local attempt.
            // It cannot clear busy/error state of a newly observed native turn.
            let _=generation;
            return step;
        }
        if self.retry.pending.as_ref().is_some_and(|pending|!matches!(pending.phase,Phase::Blocked)&&Instant::now() >= pending.deadline) {
            return self.block_retry_reconciliation("Exact Retry timed out before dispatch; no prompt was sent");
        }
        Step::default()
    }
    pub(super) fn retry_observe_frame(&mut self, frame:&Value) -> Step {
        if self.retry.pending.is_none() { return Step::default(); }
        let pending=self.retry.pending.as_ref().unwrap();
        if frame["type"]=="conversation_reset" || (frame["type"]!="prompt_suggestion" && frame["session_id"].as_str().is_some_and(|session|session!=pending.session)) {
            return self.drop_original_retry("The native conversation identity changed before Retry dispatch").unwrap();
        }
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
        let pending=self.retry.pending.as_ref().unwrap();
        if matches!(pending.phase,Phase::Blocked) {
            let request=pending.request.clone();self.controls.remove(&request);
            self.retry.pending.as_mut().unwrap().request.clear();
            return Ok(Some(Step {events:vec![Event::OriginalRetryState {pending:true,diagnostic:Some("The prior native setter settled; refresh to reconcile effective settings before sending input".into())}],..Step::default()}));
        }
        if Instant::now() >= pending.deadline {
            // This exact receipt is already here, even if the budget expired
            // inside this frame batch. Do not wait for an ACK consumed below.
            let request=pending.request.clone();
            let blocked=self.block_retry_reconciliation("Exact Retry timed out before dispatch; no prompt was sent");
            self.controls.remove(&request);
            self.retry.pending.as_mut().unwrap().request.clear();
            return Ok(Some(blocked));
        }
        if self.session_id.as_deref()!=Some(pending.session.as_str()) || self.original_retry_session()!=Some(pending.session.as_str()) {
            return Ok(self.drop_original_retry("The confirmed native session changed before Retry dispatch"));
        }
        let guard_valid=!self.busy && self.session_id.as_deref()==Some(pending.session.as_str())
            && self.original_retry_session()==Some(pending.session.as_str())
            && self.attention_generation==pending.generation && self.originals.revision==pending.revision
            && self.pending.is_empty() && self.queued.is_empty() && self.submissions.is_empty()
            && self.background_agents.is_empty();
        if !guard_valid { self.cancel_original_retry("Retry dispatch guard changed; no prompt was sent"); }
        let successful=frame["response"]["subtype"]=="success";
        let pending=self.retry.pending.as_ref().unwrap();
        if !successful && matches!(pending.phase,Phase::Applied|Phase::Policy) {
            return Ok(Some(self.block_retry_reconciliation("Claude refused effective settings readback; no prompt was sent")));
        }
        if !successful { self.cancel_original_retry("Claude refused exact settings restoration; no prompt was sent"); }
        let request=self.retry.pending.as_ref().unwrap().request.clone();self.controls.remove(&request);
        let pending=self.retry.pending.as_mut().unwrap();
        // After cancellation or a setter refusal, wait for the in-flight
        // setter receipt, then stop restoring and reconcile actual values.
        if pending.cancelled.is_some() && matches!(pending.phase,Phase::Model|Phase::Reasoning|Phase::Fast|Phase::Permission) {
            pending.phase=Phase::Permission;
        }
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
                if !valid && pending.cancelled.is_none() {
                    self.cancel_original_retry("Claude effective model or effort differs from the original; no prompt was sent");
                }
                let pending=self.retry.pending.as_mut().unwrap();
                if model_effort.is_err() {
                    return Ok(Some(self.block_retry_reconciliation("Claude did not report effective model and effort; no prompt was sent")));
                }
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
                let can_dispatch=response["session_state"]=="idle" && effective.as_ref().ok()==Some(&pending.settings);
                if !can_dispatch && pending.cancelled.is_none() {
                    self.cancel_original_retry("Claude did not confirm idle state and exact original policy; no prompt was sent");
                }
                let effective=match effective {
                    Ok(settings)=>settings,
                    Err(_)=>return Ok(Some(self.block_retry_reconciliation("Claude did not report effective permission and fast policy; no prompt was sent"))),
                };
                let pending=self.retry.pending.take().unwrap();
                self.original_connection_policy(response);
                let mut metadata=Step::default();
                if let Setting::Value(model)=&effective.model {self.selected=Some(model.clone());}
                if let Setting::Value(reasoning)=&effective.reasoning {self.reasoning=Some(reasoning.clone());}
                self.metadata(&Value::Null,&mut metadata);
                self.fast_mode_metadata(response,&mut metadata);
                if let Setting::Value(mode)=&effective.permission {metadata.events.push(Event::PermissionMode(Some(mode.clone())));}
                metadata.events.push(Event::OriginalRetryState {pending:false,diagnostic:pending.cancelled.clone().map(|reason|format!("{reason}. Effective native settings have been refreshed"))});
                if pending.cancelled.is_some() {return Ok(Some(metadata));}
                let id=pending.id.unwrap();
                self.originals.staged=Some(id.clone());
                self.originals.verified_retry=Some((id.clone(),pending.settings));
                self.attachment_names=pending.names;
                match self.command(pending.prompt.unwrap()) {
                    Ok(mut dispatched) => {
                        metadata.writes.append(&mut dispatched.writes);
                        metadata.events.append(&mut dispatched.events);
                    }
                    Err(_) => {
                        self.originals.staged=None;
                        self.originals.verified_retry=None;
                        self.attachment_names.clear();
                        self.submissions.remove(&id);
                        self.queued.retain(|message|message.id!=id);
                        self.turn_inputs.discard_undispatched(&id);
                        self.originals.updates.push(OriginalUpdate::Rejected {command_id:id});
                        metadata.events.push(Event::OriginalRetryState {pending:false,diagnostic:Some("Exact Retry input could not be encoded; no prompt was sent".into())});
                    }
                }
                return Ok(Some(metadata));
            }
            Phase::Blocked=>return Ok(None),
        };
        match self.control(next) {
            Ok(step) => {
                self.retry.pending.as_mut().unwrap().request=step.writes[0]["request_id"].as_str().unwrap().into();
                Ok(Some(step))
            }
            Err(_) => Ok(Some(self.block_retry_reconciliation("Claude Retry control admission failed; no prompt was sent"))),
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
                assert!(refused.writes.is_empty());
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
            assert!(refused.writes.is_empty());assert!(matches!(adapter.retry.pending.as_ref().unwrap().phase,Phase::Blocked));
        }
    }
    #[test]
    fn retry_timeout_fences_unsettled_setter_before_refresh_and_never_sends_late_input() {
        let mut adapter=ready_adapter();let first=begin(&mut adapter);let late=response(&first,json!({}));
        adapter.retry.pending.as_mut().unwrap().deadline=Instant::now();
        let refused=adapter.maintenance().unwrap();assert!(refused.writes.is_empty());
        assert!(adapter.command(Command::Refresh).is_err(),"unsettled setter must fence readback");
        assert!(adapter.receive(late).unwrap().writes.is_empty());
        let refreshed=adapter.command(Command::Refresh).unwrap();
        assert_eq!(refreshed.writes[0]["request"]["subtype"],"get_settings");
        let policy_read=adapter.receive(response(&refreshed,json!({"applied":{"model":"partially-restored","effort":"high"}}))).unwrap();
        let reconciled=adapter.receive(response(&policy_read,json!({"current_permission_mode":"bypassPermissions","fast_mode_state":"on","session_state":"idle"}))).unwrap();
        assert!(reconciled.writes.is_empty());assert!(adapter.retry.pending.is_none());
        assert_eq!(adapter.selected.as_deref(),Some("partially-restored"));
        assert_eq!(adapter.reasoning.as_deref(),Some("high"));
        assert!(adapter.take_original_updates().iter().any(|update|matches!(update,OriginalUpdate::Rejected{..})));
    }
    #[test]
    fn retry_deadline_expiring_inside_matching_setter_reply_consumes_that_exact_receipt() {
        let mut adapter=ready_adapter();let first=begin(&mut adapter);
        adapter.retry.pending.as_mut().unwrap().deadline=Instant::now();
        let blocked=adapter.receive(response(&first,json!({}))).unwrap();
        assert!(blocked.writes.is_empty());
        let refresh=adapter.command(Command::Refresh).unwrap();
        assert_eq!(refresh.writes[0]["request"]["subtype"],"get_settings");
    }
    #[test]
    fn retry_setter_refusal_reconciles_partial_native_settings_without_provider_error() {
        let mut adapter=ready_adapter();let first=begin(&mut adapter);
        let mut refused=response(&first,json!({}));refused["response"]["subtype"]=json!("error");
        let refresh=adapter.receive(refused).unwrap();
        assert_eq!(refresh.writes[0]["request"]["subtype"],"get_settings");
        let final_read=adapter.receive(response(&refresh,json!({"applied":{"model":"partially-restored","effort":"high"}}))).unwrap();
        let reconciled=adapter.receive(response(&final_read,policy())).unwrap();
        assert!(reconciled.writes.is_empty());assert!(adapter.retry.pending.is_none());
        assert_eq!(adapter.selected.as_deref(),Some("partially-restored"));
        assert!(reconciled.events.iter().all(|event|!matches!(event,Event::Error{..}|Event::TurnStarted|Event::TurnFinished{..})));
    }
    #[test]
    fn retry_interrupt_close_and_newer_native_work_retire_exact_unsent_intent() {
        for action in ["interrupt","close","native"] {
            let mut adapter=ready_adapter();let first=begin(&mut adapter);let late=response(&first,json!({}));
            let stopped=match action {
                "interrupt"=>adapter.command(Command::Interrupt).unwrap(),
                "close"=>adapter.discard_undispatched().unwrap().unwrap(),
                _=>adapter.receive(json!({"type":"system","subtype":"session_state_changed","session_id":SESSION,"state":"running"})).unwrap(),
            };
            assert!(stopped.writes.is_empty());
            assert!(stopped.events.iter().all(|event|!matches!(event,Event::Error{..})));
            let read=adapter.receive(late).unwrap();
            if action=="close" { assert!(read.writes.is_empty()); }
            else {
                assert_eq!(read.writes[0]["request"]["subtype"],"get_settings");
                let final_read=adapter.receive(response(&read,json!({"applied":{"model":"original-model","effort":"low"}}))).unwrap();
                assert!(adapter.receive(response(&final_read,policy())).unwrap().writes.is_empty());
            }
            assert!(adapter.retry.pending.is_none());
            assert!(adapter.take_original_updates().iter().any(|update|matches!(update,OriginalUpdate::Rejected{command_id} if command_id=="exact-retry-command")));
            assert_eq!(adapter.busy,action=="native");
        }
    }
    #[test]
    fn retry_rejects_new_queued_input_until_effective_settings_reconciliation() {
        let mut adapter=ready_adapter();let first=begin(&mut adapter);
        assert!(adapter.command(prompt(PromptMode::Queue)).is_err());
        let pending=adapter.maintenance().unwrap();assert!(pending.writes.is_empty());
        assert!(pending.events.iter().any(|event|matches!(event,Event::OriginalRetryState{pending:true,..})));
        let read=adapter.receive(response(&first,json!({}))).unwrap();
        let final_read=adapter.receive(response(&read,json!({"applied":{"model":"original-model","effort":"low"}}))).unwrap();
        assert!(adapter.receive(response(&final_read,policy())).unwrap().writes.is_empty());
        let queued=adapter.command(prompt(PromptMode::Queue)).unwrap();
        assert_eq!(queued.writes.len(),1);assert_eq!(queued.writes[0]["type"],"user");
        assert_ne!(queued.writes[0]["uuid"],"exact-retry-command");
    }
}
