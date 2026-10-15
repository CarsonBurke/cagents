use super::*;
use crate::{agent::Provider,worktree_control::{Operation,Reply}};

#[derive(Debug, Subcommand)]
pub enum WorktreeCommand {
    /// Create a journaled worktree using the application's location and setup settings.
    Create {
        branch: String,
        #[arg(long)]
        base: Option<String>,
        /// Project ID, directory or exact name of an open project; defaults to this checkout.
        #[arg(long)]
        project: Option<PathBuf>,
        /// Link this Linear issue (for example ENG-123) to the new worktree.
        #[arg(long)]
        linear: Option<String>,
        /// Skip the project's file copies and setup commands.
        #[arg(long)]
        no_setup: bool,
        /// Start a provider agent in the new worktree after setup.
        #[arg(long,value_parser=agents::parse_provider)]
        spawn_agent: Option<Provider>,
        /// Initial instruction for --spawn-agent.
        #[arg(long,requires="spawn_agent",default_value="Inspect this worktree and await further instructions.")]
        prompt: String,
        #[arg(long,requires="spawn_agent")]
        model: Option<String>,
        #[arg(long,requires="spawn_agent")]
        reasoning_effort: Option<String>,
    },
    /// List branches, paths, dirty state, upstream comparison and live sessions.
    List {
        #[arg(long)]
        project: Option<PathBuf>,
    },
    /// Remove a linked worktree after checking all local work.
    Remove {
        /// Exact branch name or worktree path.
        target: String,
        #[arg(long)]
        project: Option<PathBuf>,
        /// Confirm discarding modified, untracked, ignored and unpushed work.
        #[arg(long)]
        force: bool,
        /// Delete the branch with Git's safe -d check after removing the worktree.
        #[arg(long)]
        delete_branch: bool,
    },
}

pub(super) fn execute(command:WorktreeCommand)->Result<CommandOutput,AppError> {
    let socket=control_daemon_socket()?;
    let mut client=ControlClient::connect(&socket).map_err(|error|AppError::new("daemon_connection",error))?;
    let model=client.read_workspace_model().map_err(protocol_error)?;
    let project=match &command {
        WorktreeCommand::Create {project,..}|WorktreeCommand::List {project}|WorktreeCommand::Remove {project,..}=>project.clone(),
    };
    let project=project.map(|given| {
        if let Some(id)=given.to_str().and_then(|value|value.parse::<crate::model::ProjectId>().ok()) {
            return match client.device_operation(crate::device_service::Operation::Worktree {operation:Operation::ResolveProject {id}}).map_err(protocol_error)? {
                crate::device_service::Reply::Worktree(Reply::ResolvedProject {repository})=>Ok(repository),
                _=>Err(AppError::new("protocol","Unexpected project-ID response")),
            };
        }
        let matching=model.projects.iter().filter(|project| Path::new(&project.name)==given).collect::<Vec<_>>();
        match matching.as_slice() { [project]=>Ok(project.repository.clone()), []=>Ok(given), _=>Err(AppError::new("project","This project name is ambiguous; use its path")) }
    }).transpose()?;
    let repository=resolve_repository(project,Some(&model))?;
    let (operation,spawn)=match command {
        WorktreeCommand::Create {branch,base,linear,no_setup,spawn_agent,prompt,model,reasoning_effort,..}=>(Operation::Create {repository:repository.clone(),branch,base,linear,no_setup},spawn_agent.map(|provider|(provider,prompt,model,reasoning_effort))),
        WorktreeCommand::List {..}=>(Operation::List {repository:repository.clone()},None),
        WorktreeCommand::Remove {target,force,delete_branch,..}=> {
            let target=if !crate::device::is_remote() {match std::fs::canonicalize(&target) {Ok(path)=>path.into_os_string().into_string().map_err(|_|AppError::new("worktree_path","The selected worktree path is not UTF-8; refusing a lossy removal target"))?,Err(_)=>target}} else {target};
            (Operation::Remove {repository:repository.clone(),target,force,delete_branch,expected_fingerprint:None},None)
        },
    };
    let response=client.device_operation(crate::device_service::Operation::Worktree {operation}).map_err(protocol_error)?;
    let crate::device_service::Reply::Worktree(reply)=response else {return Err(AppError::new("protocol","Unexpected worktree response"));};
    let (command,data,human)=match reply {
        Reply::Created {worktree,warnings,setup}=>{
            let mut data=json!({"schema_version":1,"worktree":worktree,"warnings":warnings,"setup":setup});
            let mut human=format!("Created {} at {}",worktree.branch.as_deref().unwrap_or("detached"),worktree.path.display());
            for warning in &warnings {human.push_str(&format!("\nWarning: {warning}"));}
            if let Some(terminal)=setup.as_ref().and_then(|setup|setup.terminal.as_ref()) {
                human.push_str(&format!("\nWorktree setup terminal {}",terminal.id));
            }
            if let Some((provider,prompt,model,reasoning_effort))=spawn {
                if let Some(error)=setup.as_ref().and_then(|setup|setup.error.as_ref()) {
                    return Err(AppError {kind:"worktree_setup",message:format!("Worktree was created at {}, but setup failed and its agent was not started: {error}",worktree.path.display()),details:Some(data)});
                }
                if let Some(terminal)=setup.as_ref().and_then(|setup|setup.terminal.as_ref()) {
                    wait_for_setup(&mut client,terminal.id).map_err(|error|AppError { kind:"worktree_setup",message:format!("Worktree was created at {}, but its agent was not started: {error}",worktree.path.display()),details:Some(data.clone()) })?;
                }
                use crate::agent_execution::{Destination,Operation as AgentOperation,Reply as AgentReply,Spec,Submit,Surface,Target};
                // The new project and exact worktree view must exist before the
                // delegated agent is accepted, including calls made without a UI.
                client.edit_workspace_model(ModelEdit::ReopenProject {repository:repository.clone(),name:repository_display_name(&repository),kind:crate::model::ProjectKind::Git}).map_err(protocol_error)?;
                let accepted=client.agent(Authority::from_environment(),AgentOperation::Submit(Submit {
                    name:None,destination:Destination::New(Spec {
                        provider,account:None,target:Target {repository:repository.clone(),directory:worktree.path.clone(),placement:Some(crate::workspace_model::views::Scope::Worktree {repository,path:worktree.path.clone()})},
                        surface:Surface::Chat,model,permission_mode:None,reasoning_effort,
                    }),prompt,idempotency_key:uuid::Uuid::new_v4().to_string(),
                })).map_err(|error|AppError {kind:"worktree_agent",message:format!("Worktree was created at {}, but its agent could not start: {error}",worktree.path.display()),details:Some(data.clone())})?;
                let AgentReply::Accepted(accepted)=accepted else {return Err(AppError::new("protocol","Unexpected agent acceptance"));};
                human.push_str(&format!("\nAgent {}",accepted.agent.id));
                data["agent"]=serde_json::to_value(accepted).map_err(|error|AppError::new("serialize",error))?;
            }
            ("worktree.create",data,human)
        }
        Reply::Listed {worktrees}=>{
            let rows=worktrees.iter().map(|entry|vec![entry.path.display().to_string(),entry.branch.as_deref().unwrap_or("(detached)").into(),if entry.dirty {"dirty".into()}else{"clean".into()},entry.ahead.map(|count|format!("+{count}")).unwrap_or("-".into()),entry.behind.map(|count|format!("-{count}")).unwrap_or("-".into()),entry.live_sessions.len().to_string()]).collect();
            let human=super::format::table(&["PATH","BRANCH","STATE","AHEAD","BEHIND","SESSIONS"],rows,"No worktrees");
            ("worktree.list",json!({"schema_version":1,"worktrees":worktrees}),human)
        }
        Reply::BranchDeleted {..}|Reply::BranchDeleteRefused {..}=>return Err(AppError::new("protocol","Unexpected branch-deletion response")),
        Reply::StaleLock {..}=>return Err(AppError::new("protocol","Unexpected stale-lock response")),
        Reply::ResolvedProject {..}=>return Err(AppError::new("protocol","Unexpected project-ID response")),
        Reply::SetupIntent {..}|Reply::SetupPrepared {..}=>return Err(AppError::new("protocol","Unexpected setup recovery response")),
        Reply::DisposableIgnoredDirectories {..}=>return Err(AppError::new("protocol","Unexpected policy response")),
        Reply::Inspected {..}=>return Err(AppError::new("protocol","Unexpected worktree inspection response")),
        Reply::Removed {path,branch_deleted,warnings,branch_error}=> {
            let data=json!({"schema_version":1,"path":path,"branch_deleted":branch_deleted,"warnings":warnings,"branch_error":branch_error});
            if let Some(error)=branch_error {return Err(AppError {kind:"worktree_branch",message:error,details:Some(data)});}
            let mut human=format!("Removed {}{}",path.display(),branch_deleted.map(|branch|format!(" and branch {branch}")).unwrap_or_default());
            for warning in warnings {human.push_str(&format!("\nWarning: {warning}"));}
            ("worktree.remove",data,human)
        }

    };
    Ok(CommandOutput {command,data,human})
}

fn wait_for_setup(client:&mut ControlClient,id:TerminalId)->Result<(),String> {
    let deadline=Instant::now()+Duration::from_secs(120);
    loop {
        let terminals=client.list().map_err(|error|error.to_string())?;
        let terminal=terminals.into_iter().find(|terminal|terminal.session.id==id).ok_or("The setup terminal is no longer available; inspect its card before starting an agent")?;
        let exit=match terminal.session.continuity {
            TerminalContinuity::Exited {last_exit}=>Some(last_exit),
            TerminalContinuity::RelaunchAvailable {last_exit,..}=>last_exit,
            TerminalContinuity::Failed {typed_error,..}=>return Err(format!("Setup terminal failed: {typed_error:?}")),
            _=>None,
        };
        if let Some(exit)=exit {
            return if exit.code==Some(0)&&exit.signal.is_none() {Ok(())} else {Err(format!("Setup failed (exit {:?}, signal {:?}); inspect Worktree setup",exit.code,exit.signal))};
        }
        if Instant::now()>=deadline {return Err("Setup is still running; inspect Worktree setup and start the agent after it finishes".into());}
        thread::sleep(Duration::from_millis(100));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn worktree_cli_flags_and_global_json_parse_without_starting_services() {
        for arguments in [
            vec!["cagents","worktree","create","feat-x","--base","main","--linear","ENG-123","--no-setup","--spawn-agent","codex","--model","gpt-6-luna","--reasoning-effort","low","--json"],
            vec!["cagents","worktree","list","--json"],
            vec!["cagents","worktree","remove","feat-x","--force","--delete-branch","--json"],
        ] {
            let cli=Cli::try_parse_from(arguments).unwrap();assert!(cli.json);assert!(matches!(cli.command,TopCommand::Worktree {..}));
        }
    }
}
