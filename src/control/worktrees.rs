use super::*;
use crate::{agent::Provider,worktree_control::{self,Operation,Reply}};

#[derive(Debug, Subcommand)]
pub enum WorktreeCommand {
    /// Create a journaled worktree using the application's location and setup settings.
    Create {
        branch: String,
        #[arg(long)]
        base: Option<String>,
        /// Project directory or the exact name of an open project; defaults to this checkout.
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
    let socket=terminal::ensure_daemon_ready().map_err(|error|AppError::new("daemon_start",error))?;
    let mut client=ControlClient::connect(&socket).map_err(|error|AppError::new("daemon_connection",error))?;
    let model=client.read_workspace_model().map_err(protocol_error)?;
    let project=match &command {
        WorktreeCommand::Create {project,..}|WorktreeCommand::List {project}|WorktreeCommand::Remove {project,..}=>project.clone(),
    };
    let project=project.map(|given| {
        let matching=model.projects.iter().filter(|project| Path::new(&project.name)==given).collect::<Vec<_>>();
        match matching.as_slice() { [project]=>Ok(project.repository.clone()), []=>Ok(given), _=>Err(AppError::new("project","This project name is ambiguous; use its path")) }
    }).transpose()?;
    let repository=resolve_repository(project,Some(&model))?;
    let (operation,spawn)=match command {
        WorktreeCommand::Create {branch,base,linear,no_setup,spawn_agent,prompt,model,reasoning_effort,..}=>(Operation::Create {repository:repository.clone(),branch,base,linear,no_setup},spawn_agent.map(|provider|(provider,prompt,model,reasoning_effort))),
        WorktreeCommand::List {..}=>(Operation::List {repository:repository.clone()},None),
        WorktreeCommand::Remove {target,force,delete_branch,..}=>(Operation::Remove {repository:repository.clone(),target,force,delete_branch},None),
    };
    let response=client.device_operation(crate::device_service::Operation::Worktree {operation}).map_err(protocol_error)?;
    let crate::device_service::Reply::Worktree(reply)=response else {return Err(AppError::new("protocol","Unexpected worktree response"));};
    let (command,data,human)=match reply {
        Reply::Created {worktree,warnings}=>{
            let mut data=json!({"worktree":worktree,"warnings":warnings});
            let mut human=format!("Created {} at {}",worktree.branch.as_deref().unwrap_or("detached"),worktree.path.display());
            if let Some((provider,prompt,model,reasoning_effort))=spawn {
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
            let human=worktrees.iter().map(|entry|format!("{}\t{}\t{}\t{}\t{}\t{}",entry.path.display(),entry.branch.as_deref().unwrap_or("(detached)"),if entry.dirty {"dirty"}else{"clean"},entry.ahead.map(|count|format!("+{count}")).unwrap_or("-".into()),entry.behind.map(|count|format!("-{count}")).unwrap_or("-".into()),entry.live_sessions.len())).collect::<Vec<_>>().join("\n");
            ("worktree.list",json!({"worktrees":worktrees}),human)
        }
        Reply::Removed {path,branch_deleted}=>("worktree.remove",json!({"path":path,"branch_deleted":branch_deleted}),format!("Removed {}{}",path.display(),branch_deleted.map(|branch|format!(" and branch {branch}")).unwrap_or_default())),
    };
    Ok(CommandOutput {command,data,human})
}
