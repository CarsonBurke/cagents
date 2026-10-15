//! A daemon handle refers either to its local actor or the exact actor retained
//! by a native helper. Remote handles never instantiate another reducer/store.
use super::{LocalHandle,Rejection,Update,UpdateReadiness,Store};
use super::remote_actor::{RemoteActor,ActorIdentity,OutboxEntry,InputIdentity,InputReceipt};
use crate::chat::{self,Command,Snapshot,protocol::{Adapter,ProcessSpec,Step}};
use std::collections::BTreeMap;

#[derive(Clone)]
pub struct Handle(Owner);
#[derive(Clone)]
enum Owner { Local(LocalHandle), Remote(RemoteActor) }
impl Handle {
    #[cfg(test)]
    pub(crate) fn start(spec:ProcessSpec,adapter:Box<dyn Adapter>,initial:Step,store:Store,session_id:Option<String>,notify:impl Fn(Update)+Send+Sync+'static)->Result<Self,String> {
        LocalHandle::start(spec,adapter,initial,store,session_id,notify).map(|local|Self(Owner::Local(local)))
    }
    pub(crate) fn start_owned(spec:ProcessSpec,adapter:Box<dyn Adapter>,initial:Step,store:Store,source:chat::Session,recovery_environment:BTreeMap<String,String>,notify:impl Fn(Update)+Send+Sync+'static)->Result<Self,String> {
        LocalHandle::start_owned(spec,adapter,initial,store,source,recovery_environment,None,notify).map(|local|Self(Owner::Local(local)))
    }
    pub(crate) fn start_adopted(spec:ProcessSpec,adapter:Box<dyn Adapter>,initial:Step,store:Store,recovery:chat::native_holder::Recovery,holder:chat::native_holder::Holder,notify:impl Fn(Update)+Send+Sync+'static)->Result<Self,String> {
        LocalHandle::start_adopted(spec,adapter,initial,store,recovery,holder,None,notify).map(|local|Self(Owner::Local(local)))
    }
    pub(super) fn local_actor(&self)->Result<&LocalHandle,String> {match &self.0 {Owner::Local(local)=>Ok(local),Owner::Remote(_)=>Err("A retained actor must reconnect to its existing reducer; never replace it with a local provider".into())}}
    pub(crate) fn start_owned_with_previous(spec:ProcessSpec,adapter:Box<dyn Adapter>,initial:Step,store:Store,source:chat::Session,recovery_environment:BTreeMap<String,String>,previous:Option<&Handle>,notify:impl Fn(Update)+Send+Sync+'static)->Result<Self,String> {
        let previous=previous.map(Handle::local_actor).transpose()?;
        LocalHandle::start_owned(spec,adapter,initial,store,source,recovery_environment,previous,notify).map(|local|Self(Owner::Local(local)))
    }
    pub(crate) fn start_adopted_with_previous(spec:ProcessSpec,adapter:Box<dyn Adapter>,initial:Step,store:Store,recovery:chat::native_holder::Recovery,holder:chat::native_holder::Holder,previous:Option<&Handle>,notify:impl Fn(Update)+Send+Sync+'static)->Result<Self,String> {
        let previous=previous.map(Handle::local_actor).transpose()?;
        LocalHandle::start_adopted(spec,adapter,initial,store,recovery,holder,previous,notify).map(|local|Self(Owner::Local(local)))
    }
    pub(crate) fn restart_remote(&self,id:uuid::Uuid,spec:ProcessSpec,source:chat::Session,recovery_environment:BTreeMap<String,String>)->Result<super::remote_actor::RestartReceipt,String> {match &self.0 {Owner::Remote(remote)=>remote.restart(id,spec,source,recovery_environment),Owner::Local(_)=>Err("This actor is local; use its existing local restart owner".into())}}
    pub(crate) fn restart_environment(&self)->crate::terminal_ipc::ChatExecutionEnvironment {match &self.0 {Owner::Local(local)=>local.restart_environment(),Owner::Remote(remote)=>crate::terminal_ipc::ChatExecutionEnvironment(remote.restart_environment())}}
    /// The host selects this after preparing the exact authorized launch. It
    /// must drain/commit/ACK the outbox; ordinary notification is only a wakeup.
    pub(crate) fn start_remote_owned(spec:ProcessSpec,source:chat::Session,recovery_environment:BTreeMap<String,String>,notify:impl Fn(Update)+Send+Sync+'static)->Result<Self,String> {
        RemoteActor::spawn(spec,source,recovery_environment,notify).map(|remote|Self(Owner::Remote(remote)))
    }
    pub(crate) fn adopt_remote(record:&crate::daemon::holder_registry::Record,source:chat::Session,proof:crate::model::HostSessionProof,notify:impl Fn(Update)+Send+Sync+'static)->Result<Self,String> {
        RemoteActor::adopt(record,source,proof,notify).map(|remote|Self(Owner::Remote(remote)))
    }
    pub(crate) fn inspect_remote(record:&crate::daemon::holder_registry::Record,proof:crate::model::HostSessionProof)->Result<super::remote_actor::ActorInspection,String> {RemoteActor::inspect(record,proof)}
    pub(crate) fn is_remote_record(record:&crate::daemon::holder_registry::Record)->bool {RemoteActor::is_record(record)}
    pub(crate) fn wake_retained_outcomes(&self) {if let Owner::Remote(remote)=&self.0 {remote.wake_outcomes();}}
    pub(crate) fn uncommitted_outcomes(&self)->Result<Vec<OutboxEntry>,String> {match &self.0 {Owner::Remote(remote)=>remote.call("outbox_unacked",serde_json::Value::Null),_=>Err("This actor has no retained outcome outbox".into())}}
    pub(crate) fn actor_identity(&self)->Option<ActorIdentity> {match &self.0 {Owner::Remote(remote)=>Some(remote.identity()),_=>None}}
    pub(crate) fn retained_outcomes(&self,after:u64)->Result<Vec<OutboxEntry>,String> {match &self.0 {Owner::Remote(remote)=>remote.call("outbox",serde_json::json!([after])),_=>Err("This actor has no remote outbox".into())}}
    pub(crate) fn commit_retained_outcomes(&self,through:u64)->Result<(),String> {match &self.0 {Owner::Remote(remote)=>remote.call("ack_outbox",serde_json::json!([through])),_=>Err("This actor has no remote outbox".into())}}
    pub(crate) fn retained_input(&self,identity:InputIdentity)->Result<Option<InputReceipt>,String> {match &self.0 {Owner::Remote(remote)=>remote.call("input_receipt",serde_json::json!([identity])),_=>Err("This actor has no remote input journal".into())}}
    pub(crate) fn pending_retained_inputs(&self)->Vec<InputIdentity> {match &self.0 {Owner::Remote(remote)=>remote.pending_inputs(),_=>Vec::new()}}
    pub(crate) fn retry_retained_input(&self,identity:&InputIdentity)->Result<InputReceipt,String> {match &self.0 {Owner::Remote(remote)=>remote.retry_input(identity),_=>Err("This actor has no retained input envelope".into())}}
    pub(crate) fn commit_retained_inputs(&self,through:u64)->Result<(),String> {match &self.0 {Owner::Remote(remote)=>remote.call("ack_inputs",serde_json::json!([through])),_=>Err("This actor has no remote input journal".into())}}
    pub(crate) fn same_actor(&self,other:&Self)->bool {match (&self.0,&other.0) {(Owner::Local(a),Owner::Local(b))=>a.same_actor(b),(Owner::Remote(a),Owner::Remote(b))=>a.identity()==b.identity(),_=>false}}
    pub(crate) fn enqueue_command(&self,command:Command)->Result<crossbeam_channel::Receiver<Result<(),String>>,String> {match &self.0 {Owner::Local(local)=>local.enqueue_command(command),Owner::Remote(remote)=>remote.enqueue(None,command)}}
    pub fn command(&self,command:Command)->Result<(),String> {self.try_command(command).map_err(String::from)}
    pub(crate) fn try_command(&self,command:Command)->Result<(),Rejection> {match &self.0 {Owner::Local(local)=>local.try_command(command),Owner::Remote(remote)=>remote.submit(None,command)}}
    pub(crate) fn command_for_run(&self,run:crate::model::RunId,command:Command)->Result<(),String> {match &self.0 {Owner::Local(local)=>local.command_for_run(run,command),Owner::Remote(remote)=>remote.submit(Some(run),command).map_err(String::from)}}
    pub(crate) fn pinned_execution_directory(&self,candidate:&std::path::Path)->Result<std::sync::Arc<super::super::protocol::ExecutionContext>,String> {match &self.0 {Owner::Local(local)=>local.pinned_execution_directory(candidate),Owner::Remote(remote)=>remote.pinned_directory(candidate)}}
    pub(crate) fn capture_execution_directory(&self,candidate:&std::path::Path)->Result<super::DirectoryProof,String> {match &self.0 {Owner::Local(local)=>local.capture_execution_directory(candidate),Owner::Remote(remote)=>remote.capture_directory(candidate)}}
    pub(crate) fn relocate_execution_directory(&self,candidate:&std::path::Path)->Result<(),String> {match &self.0 {Owner::Local(local)=>local.relocate_execution_directory(candidate),Owner::Remote(remote)=>remote.relocate_directory(candidate)}}
    pub(crate) fn close_observation(&self)->super::CloseObservation {match &self.0 {Owner::Local(local)=>local.close_observation(),Owner::Remote(remote)=>remote.state().close.clone()}}
    pub(crate) fn close_if_revision(&self,revision:u64)->Result<(),String> {match &self.0 {Owner::Local(local)=>local.close_if_revision(revision),Owner::Remote(remote)=>remote.close_method("close_if_revision",serde_json::json!([revision]))}}
    pub(crate) fn close_if_idle(&self,generation:u64)->Result<(),String> {match &self.0 {Owner::Local(local)=>local.close_if_idle(generation),Owner::Remote(remote)=>remote.close_method("close_if_idle",serde_json::json!([generation]))}}
    pub(crate) fn close_if_idle_for_resume(&self,generation:u64)->Result<(),String> {match &self.0 {Owner::Local(local)=>local.close_if_idle_for_resume(generation),Owner::Remote(remote)=>remote.close_method("close_if_idle",serde_json::json!([generation]))}}
    pub(crate) fn check_update_readiness(&self)->Result<UpdateReadiness,String> {match &self.0 {Owner::Local(local)=>local.check_update_readiness(),Owner::Remote(remote)=>remote.call("readiness",serde_json::Value::Null)}}
    pub(crate) fn interrupt_run(&self,run:crate::model::RunId)->Result<(),String> {self.interrupt_owned(Some(run),false)}
    pub(crate) fn interrupt_owned(&self,run:Option<crate::model::RunId>,including_subagents:bool)->Result<(),String> {match &self.0 {Owner::Local(local)=>local.interrupt_owned(run,including_subagents),Owner::Remote(remote)=>remote.interrupt(run,including_subagents)}}
    pub(crate) fn update_native_source(&self,source:chat::Session,execution_console:Option<crate::model::LaunchRecipe>,paired_holder:Option<crate::terminal::holder::Record>)->Result<(),String> {match &self.0 {Owner::Local(local)=>local.update_native_source(source,execution_console,paired_holder),Owner::Remote(remote)=>remote.call("update_source",serde_json::json!([source,execution_console,paired_holder]))}}
    pub fn set_visible(&self,visible:bool) {match &self.0 {Owner::Local(local)=>local.set_visible(visible),Owner::Remote(remote)=>remote.set_visible(visible)}}
    pub(crate) fn observe_next_update(&self)->u64 {match &self.0 {Owner::Local(local)=>local.observe_next_update(),Owner::Remote(remote)=>remote.observe_next_update()}}
    pub(crate) fn file_query(&self,command:&Command)->Result<Option<chat::FileChangeReply>,String> {match &self.0 {Owner::Local(local)=>local.file_query(command),Owner::Remote(remote)=>remote.call("file_query",serde_json::json!([command]))}}
    pub(crate) fn provider_exit_output(&self,log_id:&str,offset:u64)->Result<(Vec<u8>,Option<u64>,u64),String> {match &self.0 {Owner::Local(local)=>local.provider_exit_output(log_id,offset),Owner::Remote(remote)=>remote.call("exit_output",serde_json::json!([log_id,offset]))}}
    pub(crate) fn usage(&self)->chat::Usage {match &self.0 {Owner::Local(local)=>local.usage(),Owner::Remote(remote)=>remote.state().usage.clone()}}
    pub(crate) fn continuity_loss(&self)->Option<super::remote_actor::ContinuityLoss> {match &self.0 {Owner::Local(_)=>None,Owner::Remote(remote)=>remote.state().continuity_loss.clone()}}
    pub(crate) fn ack_continuity_loss(&self,id:uuid::Uuid)->Result<(),String> {match &self.0 {Owner::Local(_)=>Err("This local actor has no retained continuity loss receipt".into()),Owner::Remote(remote)=>remote.call("ack_loss",serde_json::json!([id]))}}
    pub(crate) fn retained_runs(&self)->Vec<crate::model::RunId> {match &self.0 {Owner::Local(_)=>Vec::new(),Owner::Remote(remote)=>remote.state().retained_runs.clone()}}
    pub(crate) fn attachment_snapshot(&self)->Snapshot {match &self.0 {Owner::Local(local)=>local.snapshot(None).expect("unconditional local snapshot"),Owner::Remote(remote)=>remote.attachment_snapshot()}}
    pub fn snapshot(&self,after:Option<u64>)->Option<Snapshot> {match &self.0 {Owner::Local(local)=>local.snapshot(after),Owner::Remote(remote)=>remote.snapshot(after)}}
    pub(crate) fn presentation(&self,chat_id:crate::model::TerminalId,after:Option<u64>)->Option<crate::terminal_ipc::ServerResponse> {match &self.0 {Owner::Local(local)=>local.presentation(chat_id,after),Owner::Remote(remote)=>remote.call("presentation",serde_json::json!([chat_id,after])).ok().flatten()}}
    pub fn session_id(&self)->Option<String> {match &self.0 {Owner::Local(local)=>local.session_id(),Owner::Remote(remote)=>remote.state().session_id.clone()}}
    pub(crate) fn confirmed_session_id(&self)->Option<String> {match &self.0 {Owner::Local(local)=>local.confirmed_session_id(),Owner::Remote(remote)=>remote.state().confirmed_session.clone()}}
    pub(crate) fn launch_account_scope(&self)->Result<&crate::account::scope::LaunchAccountScope,&str> {match &self.0 {Owner::Local(local)=>local.launch_account_scope(),Owner::Remote(remote)=>remote.launch_account_scope()}}
    pub(crate) fn execution_environment(&self)->crate::terminal_ipc::ChatExecutionEnvironment {match &self.0 {Owner::Local(local)=>local.execution_environment(),Owner::Remote(remote)=>remote.execution_environment()}}
    pub(crate) fn recovery_environment_matches(&self,environment:&BTreeMap<String,String>)->bool {match &self.0 {Owner::Local(local)=>local.recovery_environment_matches(environment),Owner::Remote(remote)=>remote.recovery_environment_matches(environment)}}
    pub fn is_completed(&self)->bool {match &self.0 {Owner::Local(local)=>local.is_completed(),Owner::Remote(remote)=>remote.state().completed}}
    pub fn failure(&self)->Option<String> {match &self.0 {Owner::Local(local)=>local.failure(),Owner::Remote(remote)=>remote.state().failure.clone()}}
    pub fn owns_work(&self)->bool {match &self.0 {Owner::Local(local)=>local.owns_work(),Owner::Remote(remote)=>remote.owns_work()}}
    pub(crate) fn activity(&self)->crate::agent::ActivityState {match &self.0 {Owner::Local(local)=>local.activity(),Owner::Remote(remote)=>remote.state().activity}}
    pub(crate) fn update_activity(&self)->(bool,bool) {match &self.0 {Owner::Local(local)=>local.update_activity(),Owner::Remote(remote)=>{let state=remote.state();(state.working||state.waiting||!state.requests.is_empty(),state.ready)}}}
    pub(crate) fn is_idle(&self)->bool {match &self.0 {Owner::Local(local)=>local.is_idle(),Owner::Remote(remote)=>remote.state().idle}}
    pub(crate) fn requests(&self)->Vec<chat::Request> {match &self.0 {Owner::Local(local)=>local.requests(),Owner::Remote(remote)=>remote.state().requests.clone()}}
    pub(crate) fn background_subagents(&self)->Vec<String> {match &self.0 {Owner::Local(local)=>local.background_subagents(),Owner::Remote(remote)=>remote.state().background_subagents.clone()}}
    pub(crate) fn interrupt_subagents(&self)->Vec<String> {match &self.0 {Owner::Local(local)=>local.interrupt_subagents(),Owner::Remote(remote)=>remote.state().interrupt_subagents.clone()}}
    pub(crate) fn title(&self)->Option<String> {match &self.0 {Owner::Local(local)=>local.title(),Owner::Remote(remote)=>remote.state().title.clone()}}
    #[cfg(test)]
    pub(crate) fn resource_lifetime(&self)->Box<dyn Fn()->bool> {match &self.0 {Owner::Local(local)=>Box::new(local.resource_lifetime()),Owner::Remote(remote)=>remote.resource_lifetime()}}
    pub(crate) fn commit_native_installation(&self)->Result<(),String> {match &self.0 {Owner::Local(local)=>{local.commit_native_installation();Ok(())},Owner::Remote(remote)=>remote.commit_installation()}}
    pub(crate) fn native_continuity(&self)->chat::NativeContinuity {match &self.0 {Owner::Local(local)=>local.native_continuity(),Owner::Remote(remote)=>remote.state().native_continuity.clone()}}
    pub(crate) fn preserve_for_restart(&self)->Result<(),String> {match &self.0 {Owner::Local(local)=>local.preserve_for_restart(),Owner::Remote(remote)=>remote.preserve()}}
    pub(crate) fn close_for_resume(&self)->Result<(),String> {match &self.0 {Owner::Local(local)=>local.close_for_resume(),Owner::Remote(remote)=>remote.close()}}
    pub fn close(&self)->Result<(),String> {match &self.0 {Owner::Local(local)=>local.close(),Owner::Remote(remote)=>remote.close()}}
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn remote_ownership_is_authoritative_even_when_completion_is_cached() {
        let remote=RemoteActor::completed_failure_fixture();
        let handle=Handle(Owner::Remote(remote));
        assert!(handle.is_completed());
        assert!(handle.failure().is_some());
        assert!(handle.owns_work());
    }
}
