//! A merged PR may offer the ordinary worktree deletion dialog. Opening that
//! dialog does not remove anything, and its own fresh safety check still wins.
use super::*;
use crate::pull_request_review::CleanupHead;

impl Cagents {
    pub(super) fn open_merged_pull_request_cleanup(&mut self,card:CardId,project:ProjectId,head:CleanupHead)->Task<Message>{
        let (Some(project_row),Some(CardRuntime::PullRequests(pane)))=(self.sidebar_model.project(project),self.cards.get(&card))else{return Task::none();};
        let repository=project_row.repository.clone();let generation=pane.review.selection_generation();let requested=head.clone();
        cosmic::task::future(async move{
            let result=tokio::task::spawn_blocking(move||{
                let cancellation=CancellationToken::new();let git=RepositoryService::system("git");
                let records=git.list_worktrees(&repository,&cancellation).map_err(|error|error.to_string())?;
                Ok(records.into_iter().skip(1).find(|record|{
                    !record.bare && record.branch.as_deref().and_then(|branch|branch.strip_prefix("refs/heads/"))==Some(requested.branch.as_str())
                        && record.head_oid.as_deref()==Some(requested.head.as_str())
                        && crate::github::worktree_head_repository(&record.path,&requested.branch,&cancellation).is_some_and(|source|source.same_repository(&requested.repository))
                }))
            }).await.unwrap_or_else(|error|Err(error.to_string()));
            Message::PullRequestCleanupReady{card,project,generation,head,result}
        })
    }
    pub(super) fn merged_pull_request_cleanup_ready(&mut self,card:CardId,project:ProjectId,generation:u64,head:CleanupHead,result:Result<Option<crate::repository::WorktreeRecord>,String>)->Task<Message>{
        let Some(CardRuntime::PullRequests(pane))=self.cards.get(&card)else{return Task::none();};
        if pane.review.selection_generation()!=generation || pane.project(&self.sidebar_model)!=Some(project) || pane.review.target().is_none_or(|target|target.pull_request.state.0!="MERGED"||target.pull_request.head_ref_oid!=head.head.as_str()||target.pull_request.head_ref_name!=head.branch||target.pull_request.head_repository_key().is_none_or(|source|!source.same_repository(&head.repository))){return Task::none();}
        match result{
            Ok(Some(record))=>{
                let Some(row)=self.sidebar_model.projects.iter_mut().find(|row|row.id==project).and_then(|row|row.worktrees.iter_mut().find(|row|row.id==record.id&&row.path==record.path))else{return Task::none();};
                row.pull_request_state=Some(sidebar::WorktreePullRequestState::Merged);
                self.handle_project_action(project_ui::Action::Command{project_id:project,worktree_id:Some(record.id),command:sidebar::ProjectCommand::Delete})
            }
            Ok(None)=>{self.status=Some(TransientNotice::warning("No managed worktree matches this PR's exact head. Keep local commits and branches until you have reviewed their state."));Task::none()}
            Err(error)=>{self.status=Some(TransientNotice::error(format!("Could not locate the merged PR worktree: {error}")));Task::none()}
        }
    }
}
