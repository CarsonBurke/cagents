use super::*;
use crate::{
    model::OperationId,
    repository::{CancellationToken, CommandSpec, ProcessExecutor, RepositoryService, SystemProcessExecutor, TrackingState, WorktreeCreationOutcome, WorktreeInput, WorktreeRecord},
};
use std::path::Path;

type Service = RepositoryService<SystemProcessExecutor>;

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
    match operation {
        Operation::List { repository } => {
            let records = service.list_worktrees(&repository,&cancellation).map_err(|error|error.to_string())?;
            let worktrees = records.iter().enumerate().filter(|(_,record)|!record.bare)
                .map(|(index,record)| { let mut entry=entry(&service,record,index==0,&cancellation)?; entry.live_sessions=live_sessions(&record.path)?; Ok(entry) }).collect::<Result<Vec<_>,String>>()?;
            Ok(Reply::Listed { worktrees })
        }
        Operation::Create { repository,branch,base,linear,no_setup } => {
            let settings_path=crate::instance::settings_path().ok_or("Settings directory is unavailable")?;
            let settings=crate::settings::load(settings_path).map_err(|error|error.to_string())?;
            let mut request=service.resolve_worktree_request(&WorktreeInput {
                repository:repository.clone(),input:branch,base:base.unwrap_or_default(),
                managed_directory:settings.managed_worktree_directory,destination:String::new(),operation_id:OperationId::new(),
            },&cancellation).map_err(|error|error.to_string())?;
            request.linear_issue=linear.map(|issue|crate::linear::context::LinkedIssue { issue_id:issue, workspace:None });
            let directories=crate::instance::project_dirs().ok_or("Data directory is unavailable")?;
            let store=crate::persistence::StateStore::open(directories.data_dir().join("worktree-operations.sqlite3"),crate::daemon::read_boot_id().map_err(|error|error.to_string())?)
                .map_err(|error|error.to_string())?;
            let report=service.create_worktree_journaled_sync(&store,&request,0,&cancellation, |_|{});
            match report.outcome {
                Some(WorktreeCreationOutcome::Created {worktree,warnings}) => {
                    // Setup integration is shared with the dialog; skipping is explicit.
                    let _ = no_setup;
                    Ok(Reply::Created { worktree:entry(&service,&worktree,false,&cancellation)?,warnings:warnings.into_iter().map(|warning|warning.reported_detail()).collect() })
                }
                _ => Err(report.detail),
            }
        }
        Operation::Remove {repository,target,force,delete_branch:delete} => {
            let records=service.list_worktrees(&repository,&cancellation).map_err(|error|error.to_string())?;
            let full_branch=format!("refs/heads/{}",target.trim_start_matches("refs/heads/"));
            let candidate=if Path::new(&target).is_absolute() { PathBuf::from(&target) } else { repository.join(&target) };
            let canonical=std::fs::canonicalize(&candidate).ok();
            let (index,record)=records.iter().enumerate().find(|(_,record)|record.branch.as_deref()==Some(&full_branch)||canonical.as_ref()==Some(&record.path))
                .ok_or("No matching worktree; use an exact branch or worktree path")?;
            if index==0||record.bare { return Err("The primary worktree cannot be removed".into()); }
            if record.lock.locked { return Err("The worktree is locked; unlock it before removing it".into()); }
            let sessions=live_sessions(&record.path)?;
            if !sessions.is_empty() { return Err(format!("{} live session(s) use this worktree; close them before removing it",sessions.len())); }
            let safety=service.worktree_safety(&record.path,&cancellation).map_err(|error|error.to_string())?;
            if safety.requires_confirmation()&&!force {
                return Err(format!("Worktree contains {} modified, {} untracked, {} notable ignored file(s), {} unpushed and {} detached-only commit(s); pass --force to confirm discarding this work",safety.modified,safety.untracked,safety.notable_ignored.len(),safety.unpushed_commits,safety.detached_only_commits));
            }
            if force&&safety.detached_only_commits>0 { service.rescue_detached_worktree(&record.path,&safety,&cancellation).map_err(|error|error.to_string())?; }
            let command=CommandSpec::new("git").cwd(&repository).args(["worktree","remove"]);
            let command=if force { command.arg("--force") } else {command};
            let command=command.arg("--").arg(&record.path).timeout(std::time::Duration::from_secs(30));
            SystemProcessExecutor.execute(&command,&cancellation).and_then(|output|output.require_success(&command.program)).map_err(|error|error.reported_detail())?;
            let branch_deleted=if delete {
                if let Some(branch)=&record.branch {
                    delete_branch(&repository,branch,false,&cancellation).map_err(|error|format!("Worktree was removed, but its branch was retained: {error}"))?;
                    Some(branch.trim_start_matches("refs/heads/").to_owned())
                } else {None}
            } else {None};
            Ok(Reply::Removed {path:record.path.clone(),branch_deleted})
        }
    }
}
