//! Daemon-owned setup intent and explicit reviewed recovery.
use super::*;
use crate::{model::OperationId, persistence::PendingOperationStatus, repository::{CreateWorktreeRequest,WorktreeOperationReport,WorktreeSetupIntent}};
use std::{collections::HashSet,sync::{LazyLock,Mutex}};

static ACTIVE:LazyLock<Mutex<HashSet<PathBuf>>>=LazyLock::new(||Mutex::new(HashSet::new()));
/// Retained through host pane publication, not merely through file preparation.
/// This closes the gap in which a second resume could supersede a ready run.
pub(crate) struct SetupReservation {path:PathBuf}
impl SetupReservation {
    pub(super) fn acquire(path:&Path)->Result<Self,String> {
        if !ACTIVE.lock().unwrap_or_else(|poison|poison.into_inner()).insert(path.to_owned()) {
            return Err("Setup is already being prepared for this worktree; wait for its terminal or interrupted result".into());
        }
        Ok(Self {path:path.to_owned()})
    }
    fn active(path:&Path)->bool {ACTIVE.lock().unwrap_or_else(|poison|poison.into_inner()).contains(path)}
}
impl Drop for SetupReservation {
    fn drop(&mut self) {ACTIVE.lock().unwrap_or_else(|poison|poison.into_inner()).remove(&self.path);}
}

fn verified_report(service:&Service,row:&crate::persistence::PendingOperation,common:&Path,record:&WorktreeRecord,cancel:&CancellationToken)->Result<WorktreeOperationReport,String> {
    let report=service.worktree_setup_creation_from_row(row).map_err(|error|error.to_string())?;
    let request=report.request.as_ref().ok_or("The setup receipt has no request")?;
    let saved_common=serde_json::from_value::<PathBuf>(row.payload["common_dir"].clone()).map_err(|error|error.to_string())?;
    let saved_admin=serde_json::from_value::<PathBuf>(row.payload["admin_dir"].clone()).map_err(|error|error.to_string())?;
    let admin=service.git_dir(&record.path,cancel).map_err(|error|error.to_string())?;
    if saved_common.as_path()!=common||saved_admin!=admin||record.branch.as_deref()!=Some(request.branch.full_ref().as_str()) {
        return Err("The worktree's creation ownership changed; no setup was resumed".into());
    }
    Ok(report)
}

/// One repository inventory query discovers durable intents even if a crash
/// preceded terminal publication. Config/trust and filesystem copies are not
/// read here; the explicit Review action reloads those separately.
pub(super) fn list(service:&Service,store:&crate::persistence::StateStore,repository:&Path,cancel:&CancellationToken)->Result<Reply,String> {
    let common=service.common_dir(repository,cancel).map_err(|error|error.to_string())?;
    let records=service.list_worktrees(repository,cancel).map_err(|error|error.to_string())?;
    let records=records.iter().filter(|record|!record.bare).map(|record|(record.path.clone(),record)).collect::<std::collections::BTreeMap<_,_>>();
    let mut intents=Vec::new();let mut warnings=Vec::new();
    for row in store.journaled_worktree_operations().map_err(|error|error.to_string())? {
        if row.status!=PendingOperationStatus::Applied||row.action!="create"||row.entity_kind!="worktree_creation"||row.payload["request"]["setup_intent"].is_null() {continue;}
        let Some(record)=serde_json::from_value::<PathBuf>(row.payload["request"]["destination"].clone()).ok().and_then(|path|records.get(&path).copied()) else {continue;};
        match verified_report(service,&row,&common,record,cancel) {
            Ok(report)=>if let Some(intent)=report.request.as_ref().and_then(|request|request.setup_intent.as_ref()) {
                intents.push(SetupReceipt {path:record.path.clone(),creation_id:report.operation_id,intent:serde_json::to_string(intent).map_err(|error|error.to_string())?});
            },
            Err(error)=>warnings.push(format!("Could not verify retained setup for {}: {error}",record.path.display())),
        }
    }
    intents.sort_by(|left,right|left.path.cmp(&right.path));
    Ok(Reply::SetupIntents {intents,warnings})
}

/// Match a settled receipt to the currently registered checkout and admin dir.
/// A malformed unrelated row cannot hide a valid receipt for another worktree.
pub(super) fn report(service:&Service,store:&crate::persistence::StateStore,repository:&Path,path:&Path,cancel:&CancellationToken)->Result<(WorktreeOperationReport,PathBuf),String> {
    same_repository(service,repository,path,cancel)?;
    let path=path.canonicalize().map_err(|error|error.to_string())?;
    let records=service.list_worktrees(repository,cancel).map_err(|error|error.to_string())?;
    let record=records.iter().find(|record|record.path==path&&!record.bare).ok_or("The setup checkout is no longer registered")?;
    let primary=records.first().filter(|record|!record.bare).map(|record|record.path.clone()).ok_or("Setup needs a primary checkout")?;
    let common=service.common_dir(repository,cancel).map_err(|error|error.to_string())?;
    let mut matches=Vec::new();
    for row in store.journaled_worktree_operations().map_err(|error|error.to_string())? {
        if row.status!=PendingOperationStatus::Applied||row.action!="create"||row.entity_kind!="worktree_creation" {continue;}
        let destination=serde_json::from_value::<PathBuf>(row.payload["request"]["destination"].clone()).ok();
        if destination.as_ref()!=Some(&path) {continue;}
        let report=verified_report(service,&row,&common,record,cancel)?;
        let request=report.request.as_ref().ok_or("The setup receipt has no request")?;
        if request.setup_intent.is_some() {matches.push(report);}
    }
    if matches.len()!=1 {return Err("No unique daemon-owned setup intent is available for this checkout; review its original creation client".into());}
    Ok((matches.pop().expect("one receipt"),primary))
}

pub(super) fn read(service:&Service,store:&crate::persistence::StateStore,repository:&Path,path:&Path,cancel:&CancellationToken)->Result<Reply,String> {
    let (report,primary)=report(service,store,repository,path,cancel)?;
    let request=report.request.as_ref().expect("validated request");
    let intent=request.setup_intent.as_ref().expect("validated intent");
    let (plan,plan_error,trusted)=match crate::worktree_setup::load(&primary) {
        Ok(plan)=>match plan.as_ref().map(|plan|crate::worktree_setup::is_trusted(&primary,plan,cancel)).transpose() {
            Ok(trusted)=>(plan,None,trusted.unwrap_or(true)),
            Err(error)=>(plan,Some(error.to_string()),false),
        },
        Err(error)=>(None,Some(error.to_string()),false),
    };
    Ok(Reply::SetupIntent {
        creation_id:report.operation_id,primary,intent:serde_json::to_string(intent).map_err(|error|error.to_string())?,
        current_plan:plan.as_ref().map(serde_json::to_string).transpose().map_err(|error|error.to_string())?,
        plan_error,trusted,active:SetupReservation::active(&request.destination),
    })
}

pub(super) fn resume(service:&Service,store:&crate::persistence::StateStore,report:WorktreeOperationReport,primary:&Path,
    expected_run_id:Option<OperationId>,expected_plan_fingerprint:Option<String>,trust:bool,skip:bool,cancel:&CancellationToken,
)->Result<Reply,String> {
    let request=report.request.as_ref().ok_or("The setup receipt has no request")?;
    let previous=request.setup_intent.as_ref().ok_or("The setup receipt has no retained intent")?;
    if previous.run_id!=expected_run_id {return Err("The setup run changed after review; review its current intent again".into());}
    let plan=if skip {None} else {crate::worktree_setup::load(primary).map_err(|error|error.to_string())?};
    if !skip&&plan.as_ref().map(|plan|plan.fingerprint.clone())!=expected_plan_fingerprint {
        return Err("The setup configuration changed after review; review its current commands again".into());
    }
    if let Some(plan)=&plan {
        if plan.requires_trust()&&!crate::worktree_setup::is_trusted(primary,plan,cancel).map_err(|error|error.to_string())? {
            if !trust {return Err("Review and explicitly trust this exact setup configuration before resuming".into());}
            crate::worktree_setup::trust(primary,plan,cancel).map_err(|error|error.to_string())?;
        }
    }
    let updated=WorktreeSetupIntent {primary:Some(primary.to_owned()),plan:plan.clone(),skip,run_id:Some(OperationId::new()),copy_done:skip||plan.is_none()};
    let retained=service.update_worktree_setup_intent(store,report.operation_id,Some(previous),updated).map_err(|error|error.to_string())?;
    let request=retained.request.ok_or("The resumed setup receipt has no request")?;
    let mut warnings=Vec::new();
    let setup=plan.as_ref().and_then(|plan|prepare(service,store,&request,primary,&request.destination,plan,&mut warnings,cancel));
    Ok(Reply::SetupPrepared {path:request.destination,setup,warnings,run_id:request.setup_intent.as_ref().and_then(|intent|intent.run_id).ok_or("The resumed setup receipt has no run identity")?})
}

pub(super) fn prepare(service:&Service,store:&crate::persistence::StateStore,request:&CreateWorktreeRequest,primary:&Path,destination:&Path,
    plan:&crate::worktree_setup::SetupPlan,warnings:&mut Vec<String>,cancel:&CancellationToken,
)->Option<Setup> {
    let run_id=request.setup_intent.as_ref().and_then(|intent|intent.run_id);
    match crate::worktree_setup::prepare(primary,destination,plan,false,cancel) {
        Ok(prepared)=> {
            for missing in prepared.missing_patterns {warnings.push(format!("No setup source matched {missing}"));}
            for skipped in prepared.skipped_paths {warnings.push(format!("Skipped copying {}: {}",skipped.path.display(),skipped.reason));}
            let mut copied=match request.setup_intent.clone() {Some(intent)=>intent,None=>return Some(Setup {command:String::new(),run_id,error:Some("Setup has no retained intent; no command was started".into()),terminal:None})};
            copied.copy_done=true;
            match service.update_worktree_setup_intent(store,request.operation_id,request.setup_intent.as_ref(),copied) {
                Ok(_)=>prepared.command.map(|command|Setup {command,run_id,terminal:None,error:None}),
                Err(error)=> {
                    let error=format!("Setup copied files, but its progress could not be retained; no command was started: {error}");
                    warnings.push(error.clone());Some(Setup {command:String::new(),run_id,terminal:None,error:Some(error)})
                }
            }
        }
        Err(error)=> {
            let error=error.to_string();warnings.push(format!("Worktree was retained, but setup failed: {error}"));
            Some(Setup {command:String::new(),run_id,terminal:None,error:Some(error)})
        }
    }
}
