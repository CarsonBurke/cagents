use super::*;
use crate::{
    model::OperationId,
    repository::{CancellationToken, ProcessExecutor, RepositoryService, SystemProcessExecutor, TrackingState, WorktreeCreationOutcome, WorktreeInput, WorktreeRecord},
};
use std::path::Path;

type Service = RepositoryService<SystemProcessExecutor>;
// Recovery may rewrite unresolved journal evidence. It must never race a
// live create whose child currently owns that evidence.
static OPERATIONS:std::sync::RwLock<()> = std::sync::RwLock::new(());

fn entry(service: &Service, record: &WorktreeRecord, primary: bool, cancellation: &CancellationToken) -> Result<Entry,String> {
    let status = service.status(&record.path, cancellation).map_err(|error| error.to_string())?;
    let (ahead,behind) = match status.tracking {
        TrackingState::Compared { ahead, behind, .. } => (Some(ahead),Some(behind)),
        _ => (None,None),
    };
    Ok(Entry {
        path: record.path.clone(),
        branch: record.branch.as_deref().map(|branch| branch.strip_prefix("refs/heads/").unwrap_or(branch).to_owned()),
        head_oid: record.head_oid.clone(), primary, detached:record.detached, locked:record.lock.locked,
        dirty:status.dirty.is_dirty(), ahead,behind,live_sessions:Vec::new(),
    })
}

pub(crate) fn execute(operation: Operation, mut live_sessions: impl FnMut(&Path) -> Result<Vec<crate::model::TerminalId>,String>) -> Result<Reply,String> {
    let cancellation = CancellationToken::new();
    let service = RepositoryService::system("git");
    let store=journal_store()?;
    {
        let idle=match OPERATIONS.try_write() {
            Ok(guard)=>Some(guard),
            Err(std::sync::TryLockError::Poisoned(error))=>Some(error.into_inner()),
            Err(std::sync::TryLockError::WouldBlock)=>None,
        };
        if let Some(_idle)=idle {
            service.rehydrate_pending_worktree_operations(&store).map_err(|error|error.to_string())?;
        }
    }
    let _active=OPERATIONS.read().unwrap_or_else(|poison|poison.into_inner());
    match operation {
        Operation::ResolveProject {id} => {
            let mut matches=std::collections::BTreeSet::new();
            for path in crate::instance::window_database_paths().map_err(|error|error.to_string())? {
                if !path.exists() {continue;}
                if let Some(repository)=crate::persistence::read_project_repository(&path,id).map_err(|error|error.to_string())? {matches.insert(repository);}
            }
            if matches.len()!=1 {return Err(format!("Project {id} is unknown, closed or ambiguous; use its checkout path"));}
            Ok(Reply::ResolvedProject {repository:matches.into_iter().next().expect("one matching project")})
        }
        Operation::List { repository } => {
            let records = service.list_worktrees(&repository,&cancellation).map_err(|error|error.to_string())?;
            let worktrees = records.iter().enumerate().filter(|(_,record)|!record.bare)
                .map(|(index,record)| { let mut entry=entry(&service,record,index==0,&cancellation)?; entry.live_sessions=live_sessions(&record.path)?; Ok(entry) }).collect::<Result<Vec<_>,String>>()?;
            Ok(Reply::Listed { worktrees })
        }
        Operation::Create { repository,branch,base,linear,no_setup } => {
            let linear=linear.map(|issue|crate::linear::issue_reference(&issue)).transpose().map_err(|error|error.to_string())?;
            let records=service.list_worktrees(&repository,&cancellation).map_err(|error|error.to_string())?;
            let primary=records.first().filter(|record|!record.bare).map(|record|record.path.clone()).ok_or("A non-bare primary checkout is required")?;
            let setup_plan=if no_setup {None} else {crate::worktree_setup::load(&primary).map_err(|error|error.to_string())?};
            if let Some(plan)=&setup_plan {
                if plan.requires_trust()&&!crate::worktree_setup::is_trusted(&primary,plan,&cancellation).map_err(|error|error.to_string())? {
                    return Err("This project's setup command has not been trusted. Review and trust it in the new-worktree dialog, or pass --no-setup".into());
                }
            }
            let settings_path=crate::instance::settings_path().ok_or("Settings directory is unavailable")?;
            let settings=crate::settings::load(settings_path).map_err(|error|error.to_string())?;
            let mut request=service.resolve_worktree_request(&WorktreeInput {
                repository:repository.clone(),input:branch,base:base.unwrap_or_default(),
                managed_directory:settings.managed_worktree_directory,destination:String::new(),operation_id:OperationId::new(),
            },&cancellation).map_err(|error|error.to_string())?;
            request.linear_issue=linear.map(|issue|crate::linear::context::LinkedIssue { issue_id:issue, workspace:None });
            if setup_plan.is_some()||no_setup {
                request.setup_intent=Some(crate::repository::WorktreeSetupIntent {
                    primary:Some(primary.clone()),plan:setup_plan.clone(),skip:no_setup,
                    run_id:Some(OperationId::new()),copy_done:no_setup,
                });
            }
            let run_id=request.setup_intent.as_ref().and_then(|intent|intent.run_id);
            let report=service.create_worktree_journaled_sync(&store,&request,0,&cancellation, |_|{});
            match report.outcome {
                Some(WorktreeCreationOutcome::Created {worktree,warnings}) if report.state==crate::repository::WorktreeOperationState::Created => {
                    let mut warnings=warnings.into_iter().map(|warning|warning.reported_detail()).collect::<Vec<_>>();
                    let setup=if let Some(plan)=setup_plan {
                        match crate::worktree_setup::prepare(&primary,&worktree.path,&plan,false,&cancellation) {
                            Ok(prepared)=> {
                                for missing in prepared.missing_patterns { warnings.push(format!("No setup source matched {missing}")); }
                                for skipped in prepared.skipped_paths { warnings.push(format!("Skipped copying {}: {}",skipped.path.display(),skipped.reason)); }
                                let mut copied=request.setup_intent.clone().expect("setup plan has retained intent");
                                copied.copy_done=true;
                                match service.update_worktree_setup_intent(&store,request.operation_id,request.setup_intent.as_ref(),copied) {
                                    Ok(_)=>prepared.command.map(|command|Setup {command,run_id,terminal:None,error:None}),
                                    Err(error)=> {
                                        let error=format!("Setup copied files, but its progress could not be retained; no command was started: {error}");
                                        warnings.push(error.clone());
                                        Some(Setup {command:String::new(),run_id,terminal:None,error:Some(error)})
                                    }
                                }
                            }
                            Err(error)=> { let error=error.to_string(); warnings.push(format!("Worktree was created, but setup failed: {error}")); Some(Setup {command:String::new(),run_id,terminal:None,error:Some(error)}) }
                        }
                    } else {None};
                    Ok(Reply::Created { worktree:entry(&service,&worktree,false,&cancellation)?,warnings,setup })
                }
                _ => Err(report.detail),
            }
        }
        Operation::Remove {repository,target,force,delete_branch:delete,expected_fingerprint} => {
            let records=service.list_worktrees(&repository,&cancellation).map_err(|error|error.to_string())?;
            let full_branch=format!("refs/heads/{target}");
            let candidate=if Path::new(&target).is_absolute() { PathBuf::from(&target) } else {
                records.first().map(|record|record.path.join(&target)).ok_or("Repository has no primary checkout")?
            };
            let canonical=std::fs::canonicalize(&candidate).ok();
            let matches=records.iter().enumerate().filter(|(_,record)|record.branch.as_deref()==Some(&full_branch)||canonical.as_ref()==Some(&record.path)).collect::<Vec<_>>();
            let (index,record)=match matches.as_slice() {
                [record]=>*record,
                []=>return Err("No matching worktree; use an exact branch or worktree path".into()),
                _=>return Err("The target matches different branch and path worktrees; use the exact absolute path".into()),
            };
            if index==0||record.bare { return Err("The primary worktree cannot be removed".into()); }
            if record.lock.locked { return Err("The worktree is locked; unlock it before removing it".into()); }
            let sessions=live_sessions(&record.path)?;
            if !sessions.is_empty() { return Err(format!("{} live session(s) use this worktree; close them before removing it",sessions.len())); }
            let _directory=crate::worktree_cleanup::removal_protection(&record.path)
                .map_err(|error|format!("Worktree is open in a window/card, editor or setup preparation; close its scopes before removal: {error}"))?;
            let safety=service.worktree_safety(&record.path,&cancellation).map_err(|error|error.to_string())?;
            if expected_fingerprint.as_ref().is_some_and(|fingerprint|fingerprint!=&safety.fingerprint) {
                return Err("The worktree changed after confirmation; review its safety again".into());
            }
            if !force&&(safety.files_to_discard()>0||(expected_fingerprint.is_none()&&safety.requires_confirmation())) {
                return Err(format!("Worktree contains {} modified, {} untracked, {} notable ignored file(s), {} unpushed and {} detached-only commit(s); pass --force to confirm discarding this work",safety.modified,safety.untracked,safety.notable_ignored.len(),safety.unpushed_commits,safety.detached_only_commits));
            }
            if safety.detached_only_commits>0&&(force||expected_fingerprint.is_some()) { service.rescue_detached_worktree(&record.path,&safety,&cancellation).map_err(|error|error.to_string())?; }
            let removed=crate::worktree_cleanup::RemovedWorktree {
                common_dir:service.common_dir(&repository,&cancellation).map_err(|error|error.to_string())?,
                admin_dir:service.git_dir(&record.path,&cancellation).map_err(|error|error.to_string())?,
                path:record.path.clone(),branch:record.branch.clone(),
            };
            let command=super::git_command(&repository).args(["worktree","remove"]);
            let command=if force&&safety.files_to_discard()>0 { command.arg("--force") } else {command};
            let command=command.arg("--").arg(&record.path).timeout(std::time::Duration::from_secs(30));
            SystemProcessExecutor.execute(&command,&cancellation).and_then(|output|output.require_success(&command.program)).map_err(|error|error.reported_detail())?;
            let mut warnings=Vec::new();
            if let Err(failures)=crate::repository::release_deleted_worktree_creations(&store,&removed) {
                warnings.push(format!("Worktree was removed, but its creation receipt could not be retired: {}",failures.into_iter().map(|error|error.to_string()).collect::<Vec<_>>().join("; ")));
            }
            let mut branch_error=None;
            let branch_deleted=if delete {
                if let Some(branch)=&record.branch {
                    let branch=branch.strip_prefix("refs/heads/").unwrap_or(branch);
                    match delete_branch(&repository,branch,false,&cancellation) {
                        Ok(())=>Some(branch.to_owned()),
                        Err(error)=> {branch_error=Some(format!("Worktree was removed, but its branch was retained: {error}"));None}
                    }
                } else {None}
            } else {None};
            Ok(Reply::Removed {path:record.path.clone(),branch_deleted,warnings,branch_error})
        }
        Operation::StaleLock {repository,path,unlock} => {
            same_repository(&service,&repository,&path,&cancellation)?;
            if unlock {service.unlock_stale_worktree(&store,&repository,&path,&cancellation)?;}
            else {service.stale_worktree_lock(&store,&repository,&path,&cancellation)?;}
            Ok(Reply::StaleLock {unlocked:unlock})
        }

        Operation::DeleteBranch {repository,branch,force,expected_oid} => {
            if force&&expected_oid.is_none() {
                return Ok(Reply::BranchDeleteRefused {failure:BranchDeleteFailure {kind:BranchDeleteFailureKind::Other,detail:"Force deletion requires the branch commit shown in confirmation".into()}});
            }
            Ok(match delete_branch_checked_at(&repository,&branch,force,expected_oid.as_deref(),&cancellation) {
                Ok(())=>Reply::BranchDeleted {branch},
                Err(failure)=>Reply::BranchDeleteRefused {failure},
            })
        }
        Operation::Inspect {repository,path} => inspect(&service,&repository,&path,&cancellation),
        Operation::SetDisposableIgnoredDirectories {repository,path,directories} => {
            same_repository(&service,&repository,&path,&cancellation)?;
            service.set_disposable_ignored_directories(&path,&directories,&cancellation).map_err(|error|error.to_string())?;
            Ok(Reply::DisposableIgnoredDirectories {directories:service.disposable_ignored_directories(&path,&cancellation).map_err(|error|error.to_string())?})
        }
        Operation::GetDisposableIgnoredDirectories {repository,path}=> {
            same_repository(&service,&repository,&path,&cancellation)?;
            Ok(Reply::DisposableIgnoredDirectories {directories:service.disposable_ignored_directories(&path,&cancellation).map_err(|error|error.to_string())?})
        }
    }
}

fn same_repository(service:&Service,repository:&Path,path:&Path,cancel:&CancellationToken)->Result<(),String> {
    let common=service.common_dir(repository,cancel).map_err(|error|error.to_string())?;
    if service.common_dir(path,cancel).map_err(|error|error.to_string())?!=common {
        return Err("The selected worktree belongs to another repository".into());
    }
    Ok(())
}
fn inspect(service:&Service,repository:&Path,path:&Path,cancel:&CancellationToken)->Result<Reply,String> {
    same_repository(service,repository,path,cancel)?;
    let snapshot=service.worktree_safety(path,cancel).map_err(|error|error.to_string())?;
    Ok(Reply::Inspected {snapshot:serde_json::to_string(&snapshot).map_err(|error|error.to_string())?,disposable_ignored_directories:service.disposable_ignored_directories(path,cancel).map_err(|error|error.to_string())?})
}

fn journal_store()->Result<crate::persistence::StateStore,String> {
    let directories=crate::instance::project_dirs().ok_or("Data directory is unavailable")?;
    crate::persistence::StateStore::open(directories.data_dir().join("worktree-operations.sqlite3"),crate::daemon::read_boot_id().map_err(|error|error.to_string())?)
        .map_err(|error|error.to_string())
}
