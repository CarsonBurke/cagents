//! An explicit new provider incarnation inside the same retained actor owner.
//! A repeated request identity reads its receipt; it never resubmits a prompt.
use super::*;
#[derive(Clone,Debug,PartialEq,Eq,Serialize,Deserialize)]
pub(crate) enum RestartCleanup {TeardownProved,Uncertain}
#[derive(Clone,Debug,PartialEq,Eq,Serialize,Deserialize)]
pub(crate) struct RestartFailure {pub reason:String,pub cleanup:RestartCleanup}
#[derive(Clone,Debug,PartialEq,Eq,Serialize,Deserialize)]
pub(crate) enum RestartState {Accepted,Acknowledged(Result<Process,RestartFailure>)}
#[derive(Clone,Debug,Serialize,Deserialize)]
pub(crate) struct RestartReceipt {pub id:uuid::Uuid,pub state:RestartState}
#[derive(Serialize,Deserialize)]
pub(super) struct Request {id:uuid::Uuid,source:chat::Session,program:String,arguments:Vec<String>,environment:BTreeMap<String,String>,working_directory:PathBuf,recovery_environment:BTreeMap<String,String>}
impl RemoteActor {
    pub fn restart(&self,id:uuid::Uuid,mut spec:ProcessSpec,source:chat::Session,recovery_environment:BTreeMap<String,String>)->Result<RestartReceipt,String> {
        spec.remove_unset_environment();
        let request=Request {id,source,program:spec.program,arguments:spec.arguments,environment:spec.environment,working_directory:spec.working_directory,recovery_environment};
        // Caller owns this stable ID across an ambiguous reply. Repeating the
        // exact request only queries/adopts the existing helper receipt.
        let receipt:RestartReceipt=self.call("restart",serde_json::to_value(request).map_err(|error|error.to_string())?)?;
        if matches!(&receipt.state,RestartState::Acknowledged(Ok(_))) {
            if let Some(state)=self.call::<Option<Resident>>("state",json!([self.state().changed]))? {self.publish_resident(state);}
            if self.0.stop.load(Ordering::Acquire) {return Ok(receipt);}
            *self.0.environment.lock().unwrap_or_else(|p|p.into_inner())=self.call("execution_environment",Value::Null)?;
            (self.0.notify)(Update::Observed {generation:self.state().generation});(self.0.notify)(Update::Presentation);
        }
        Ok(receipt)
    }
}
impl Server {
    pub(super) fn restart(&mut self,request:Request)->Result<RestartReceipt,String> {
        let hash=*blake3::hash(&serde_json::to_vec(&request).map_err(|error|error.to_string())?).as_bytes();
        if let Some(state)=self.journal.lock().unwrap_or_else(|p|p.into_inner()).restart(request.id,&hash)? {return Ok(RestartReceipt {id:request.id,state});}
        if self.closed_snapshot.is_some() {return Err("This actor was explicitly closed; acknowledge final outcomes and retire it before creating a new session".into());}
        if !self.installed || !self.actor.is_completed() {return Err("The existing provider still owns work; explicit restart requires its completed teardown".into());}
        let mut expected=published_source(&self.source,self.actor.confirmed_session_id().as_deref(),&self.resident()?.native_continuity);
        expected.proof=Some(self.proof.clone());
        if request.source!=expected && request.source!=self.source {return Err("Restart changed the issued conversation, account, recipe or ownership proof".into());}
        if request.recovery_environment!=self.recovery_environment {return Err("Restart changed the retained account environment".into());}
        for name in ["HOME","CLAUDE_CONFIG_DIR","ANTHROPIC_API_KEY","ANTHROPIC_AUTH_TOKEN","ANTHROPIC_BASE_URL"] {
            if request.environment.get(name)!=self.environment.0.get(name) {return Err("Restart changed the actual retained provider account environment".into());}
        }
        let context=self.actor.pinned_execution_directory(&request.working_directory)?;
        self.actor.close()?;reap_orphans()?;
        self.journal.lock().unwrap_or_else(|p|p.into_inner()).clear_acknowledged_loss_for_restart()?;
        let id=request.id;
        let mut lineage=self.published_sources();if !lineage.contains(&self.source) {lineage.push(self.source.clone());}
        if lineage.len()>1024 || serde_json::to_vec(&lineage).map_err(|error|error.to_string())?.len()>4*1024*1024 {return Err("Retained source lineage capacity reached".into());}
        self.journal.lock().unwrap_or_else(|p|p.into_inner()).admit_restart(id,&hash)?;
        self.lineage=lineage;
        let previous=self.actor.clone();
        let result=self.replace_provider(request).map_err(|reason|RestartFailure {
            reason:reason.chars().take(4096).collect(),
            // start_inner without an external launch owner returns Err only
            // after proved cleanup (or before provider creation). An uncertain
            // newly created actor is returned and remains resident instead.
            cleanup:if self.actor.same_actor(&previous)||self.actor.is_completed() {RestartCleanup::TeardownProved}else {RestartCleanup::Uncertain},
        });
        let state=self.journal.lock().unwrap_or_else(|p|p.into_inner()).finish_restart(id,result)?;
        self.changed.fetch_add(1,Ordering::Release);
        // Keep the original approved descriptor live throughout both phases.
        drop(context);
        Ok(RestartReceipt {id,state})
    }
    fn replace_provider(&mut self,request:Request)->Result<Process,String> {
        let context=self.actor.pinned_execution_directory(&request.working_directory)?;
        // Adapter-owned cwd/history and initial frames must be prepared from
        // the same verified operational context as the new provider process.
        // The issued source recipe remains immutable in self.source.
        let mut operational=request.source.launch.clone();
        operational.recipe.working_directory=request.working_directory.clone();
        operational.recipe.environment=request.environment.clone();operational.recipe.unset_environment.clear();
        let (mut spec,mut adapter)=chat::protocol::create(&operational)?;
        if !matches!(spec.transport,TransportSpec::JsonLines) {return Err("Restart requires the same Claude stream-json transport".into());}
        spec.program=request.program;spec.arguments=request.arguments;spec.environment=request.environment;spec.unset_environment.clear();spec.working_directory=request.working_directory;spec.execution=Some(context.clone());spec.messages=self.mailbox_seal.clone();
        let mut initial=adapter.initialize()?;
        let validation=super::super::preflight_with_previous(&spec,&mut initial,&self.actor,false)?;
        let old=self.actor.close_observation();let old_generation=self.actor.0.state.lock().unwrap_or_else(|p|p.into_inner()).generation;
        let account_scope=self.actor.launch_account_scope().cloned().map_err(str::to_owned);
        let notify=self.notifier();
        let actor=LocalHandle::start_inner(spec.clone(),adapter,initial,validation,Some(self.actor.0.store.clone()),request.source.launch.session_id.clone(),None,false,account_scope,notify)?;
        {let mut close=actor.0.close_state.lock().unwrap_or_else(|p|p.into_inner());close.actor_id=self.identity.instance;close.revision=old.close_revision.saturating_add(1).saturating_add(close.revision);}
        {let mut state=actor.0.state.lock().unwrap_or_else(|p|p.into_inner());state.generation=old_generation.saturating_add(1).saturating_add(state.generation);state.turn_generation=old.turn_generation.saturating_add(state.turn_generation);confirm_installed_snapshot(&mut state);}
        self.environment=crate::terminal_ipc::ChatExecutionEnvironment(spec.environment);
        self.source=request.source;self._directory=context.directory_descriptor();
        self.actor=actor;
        self.changed.fetch_add(1,Ordering::Release);
        self.actor.provider_process().ok_or("The restarted provider has no verified process identity; inspect its retained failure before another restart".into())
    }
}
