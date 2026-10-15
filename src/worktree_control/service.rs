use super::*;
use crate::{
    model::OperationId,
    repository::{CancellationToken, CommandSpec, ProcessExecutor, RepositoryService, SystemProcessExecutor, TrackingState, WorktreeCreationOutcome, WorktreeInput, WorktreeRecord},
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
        branch: record.branch.as_deref().map(|branch| branch.trim_start_matches("refs/heads/").to_owned()),
        head_oid: record.head_oid.clone(), primary, detached:record.detached, locked:record.lock.locked,
        dirty:status.dirty.is_dirty(), ahead,behind,live_sessions:Vec::new(),
    })
}

pub(crate) fn execute(operation: Operation, live_sessions: impl Fn(&Path) -> Result<Vec<crate::model::TerminalId>,String>) -> Result<Reply,String> {
    let cancellation = CancellationToken::new();
    let service = RepositoryService::system("git");
    let store=journal_store()?;
    if let Ok(_idle)=OPERATIONS.try_write() {
        service.rehydrate_pending_worktree_operations(&store).map_err(|error|error.to_string())?;
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
            let report=service.create_worktree_journaled_sync(&store,&request,0,&cancellation, |_|{});
            match report.outcome {
                Some(WorktreeCreationOutcome::Created {worktree,warnings}) if report.state==crate::repository::WorktreeOperationState::Created => {
                    let mut warnings=warnings.into_iter().map(|warning|warning.reported_detail()).collect::<Vec<_>>();
                    let setup=if let Some(plan)=setup_plan {
                        match crate::worktree_setup::prepare(&primary,&worktree.path,&plan,false,&cancellation) {
                            Ok(prepared)=> {
                                for missing in prepared.missing_patterns { warnings.push(format!("No setup source matched {missing}")); }
                                for skipped in prepared.skipped_paths { warnings.push(format!("Skipped copying {}: {}",skipped.path.display(),skipped.reason)); }
                                prepared.command.map(|command|Setup {command,terminal:None,error:None})
                            }
                            Err(error)=> { let error=error.to_string(); warnings.push(format!("Worktree was created, but setup failed: {error}")); Some(Setup {command:String::new(),terminal:None,error:Some(error)}) }
                        }
                    } else {None};
                    Ok(Reply::Created { worktree:entry(&service,&worktree,false,&cancellation)?,warnings,setup })
                }
                _ => Err(report.detail),
            }
        }
        Operation::Remove {repository,target,force,delete_branch:delete,expected_fingerprint} => {
            let records=service.list_worktrees(&repository,&cancellation).map_err(|error|error.to_string())?;
            let full_branch=format!("refs/heads/{}",target.trim_start_matches("refs/heads/"));
            let candidate=if Path::new(&target).is_absolute() { PathBuf::from(&target) } else {
                records.first().map(|record|record.path.join(&target)).ok_or("Repository has no primary checkout")?
            };
            let canonical=std::fs::canonicalize(&candidate).ok();
            let (index,record)=records.iter().enumerate().find(|(_,record)|record.branch.as_deref()==Some(&full_branch)||canonical.as_ref()==Some(&record.path))
                .ok_or("No matching worktree; use an exact branch or worktree path")?;
            if index==0||record.bare { return Err("The primary worktree cannot be removed".into()); }
            if record.lock.locked { return Err("The worktree is locked; unlock it before removing it".into()); }
            let sessions=live_sessions(&record.path)?;
            if !sessions.is_empty() { return Err(format!("{} live session(s) use this worktree; close them before removing it",sessions.len())); }
            let safety=service.worktree_safety(&record.path,&cancellation).map_err(|error|error.to_string())?;
            if expected_fingerprint.as_ref().is_some_and(|fingerprint|fingerprint!=&safety.fingerprint) {
                return Err("The worktree changed after confirmation; review its safety again".into());
            }
            if safety.requires_confirmation()&&!force {
                return Err(format!("Worktree contains {} modified, {} untracked, {} notable ignored file(s), {} unpushed and {} detached-only commit(s); pass --force to confirm discarding this work",safety.modified,safety.untracked,safety.notable_ignored.len(),safety.unpushed_commits,safety.detached_only_commits));
            }
            if force&&safety.detached_only_commits>0 { service.rescue_detached_worktree(&record.path,&safety,&cancellation).map_err(|error|error.to_string())?; }
            let removed=crate::worktree_cleanup::RemovedWorktree {
                common_dir:service.common_dir(&repository,&cancellation).map_err(|error|error.to_string())?,
                admin_dir:service.git_dir(&record.path,&cancellation).map_err(|error|error.to_string())?,
                path:record.path.clone(),branch:record.branch.clone(),
            };
            let command=CommandSpec::new("git").cwd(&repository).args(["worktree","remove"]);
            let command=if force { command.arg("--force") } else {command};
            let command=command.arg("--").arg(&record.path).timeout(std::time::Duration::from_secs(30));
            SystemProcessExecutor.execute(&command,&cancellation).and_then(|output|output.require_success(&command.program)).map_err(|error|error.reported_detail())?;
            crate::repository::release_deleted_worktree_creations(&store,&removed)
                .map_err(|failures|format!("Worktree was removed, but its creation receipt could not be retired: {}",failures.into_iter().map(|error|error.to_string()).collect::<Vec<_>>().join("; ")))?;
            let branch_deleted=if delete {
                if let Some(branch)=&record.branch {
                    delete_branch(&repository,branch,false,&cancellation).map_err(|error|format!("Worktree was removed, but its branch was retained: {error}"))?;
                    Some(branch.trim_start_matches("refs/heads/").to_owned())
                } else {None}
            } else {None};
            Ok(Reply::Removed {path:record.path.clone(),branch_deleted})
        }
        Operation::Inspect {repository,path} => inspect(&service,&repository,&path,&cancellation),
        Operation::SetDisposableIgnoredDirectories {repository,path,directories} => {
            same_repository(&service,&repository,&path,&cancellation)?;
            service.set_disposable_ignored_directories(&path,&directories,&cancellation).map_err(|error|error.to_string())?;
            inspect(&service,&repository,&path,&cancellation)
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
