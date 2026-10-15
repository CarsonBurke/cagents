//! Immutable UI consent basis. No process cleanup authority is granted here.
use super::*;

#[derive(Debug, Clone)]
pub(in crate::app) struct RemovalSource {
    pub(super) registry: uuid::Uuid,
    pub(super) repository: PathBuf,
    pub(super) scope: Scope,
    pub(super) before: Pane,
    execution_instance:Option<uuid::Uuid>,
    observed_chat:Option<crate::chat::Session>,
}
impl RemovalSource {
    pub(super) fn change(&self) -> Change {
        Change::RemoveSource {registry:self.registry,repository:self.repository.clone(),before:self.before.clone(),execution_instance:self.execution_instance}
    }
    pub(super) fn current(&self,model:&crate::workspace_model::WorkspaceModel)->bool {
        removal_current(model,&self.scope,&self.change())
    }
}

pub(in crate::app) fn removal_current(model:&crate::workspace_model::WorkspaceModel,scope:&Scope,change:&Change)->bool {
    let Change::RemoveSource {registry,repository,before,..}=change else{return !matches!(change,Change::Remove {..});};
    if *registry==crate::workspace_model::NO_REGISTRY || model.identity!=*registry {return false;}
    let owner=match scope {
        Scope::Channel(id)=>model.channel(*id).map(|channel|&channel.repository),
        Scope::Worktree {repository,..}=>Some(repository),
    };
    if owner!=Some(repository) || !model.projects.iter().any(|project|&project.repository==repository) {return false;}
    let mut matches=model.views.iter().flat_map(|view|view.panes.iter().filter(move |pane|pane.id==before.id).map(move |pane|(&view.scope,pane)));
    let Some((current_scope,current))=matches.next() else{return false;};
    matches.next().is_none() && current_scope==scope && views::same_pane_source(before,current)
}

impl Cagents {
    /// Capture before asynchronous cleanup. None means an unpublished local
    /// pane; ambiguous or previously shared membership is an actionable refusal.
    pub(in crate::app) fn capture_shared_removal_source(&self,card:CardId)->Result<Option<RemovalSource>,String> {
        let model=&self.workspace_model;
        let mut owners=model.views.iter().flat_map(|view|view.panes.iter().filter(move |pane|pane.id==card).map(move |pane|(&view.scope,pane)));
        let first=owners.next();
        if owners.next().is_some(){return Err("The pane has ambiguous shared membership; reconnect before closing it.".into());}
        let Some((scope,before))=first else {
            return if self.shared_views.attached.contains(&card) || self.shared_views.baseline.values().any(|panes|panes.iter().any(|pane|pane.id==card)) {
                Err("The pane's shared source is unavailable; reconnect before closing it.".into())
            } else {Ok(None)};
        };
        let kind=self.card_state(card).map(|card|&card.kind).ok_or("The pane descriptor is unavailable.")?;
        if !super::super::terminal_close::same_removal_source(&before.kind,&views::shared_kind(kind)) || before.pending_terminal_start {
            return Err("The shared pane changed before Close; its current source was kept.".into());
        }
        let repository=match scope {
            Scope::Channel(id)=>model.channel(*id).map(|channel|channel.repository.clone()).ok_or("The pane's project is unavailable.")?,
            Scope::Worktree {repository,..}=>repository.clone(),
        };
        let source=RemovalSource {registry:model.identity,repository,scope:scope.clone(),before:before.clone(),execution_instance:self.terminal_worker_for_shared_removal(card),observed_chat:self.chat_return_session(card)};
        if !source.current(model){return Err("The shared pane source is no longer authoritative.".into());}
        Ok(Some(source))
    }
    fn shared_removal_local_source_matches(&self,card:CardId,source:&RemovalSource)->bool {
        self.card_state(card).is_some_and(|state|super::super::terminal_close::same_removal_source(&source.before.kind,&views::shared_kind(&state.kind)))
            && match (&source.observed_chat,self.chat_return_session(card)) {
                (Some(expected),Some(current))=>super::super::terminal_close::same_chat_removal_source(expected,&current),
                (None,None)=>true,
                _=>false,
            }
    }
    /// Install only the original authenticated basis; never select a replacement
    /// source after a lifecycle reply. Local drafts need no shared capability.
    pub(in crate::app) fn install_shared_removal_source(&mut self,card:CardId,source:Option<RemovalSource>)->bool {
        match source {
            Some(source) if source.before.id==card && source.current(&self.workspace_model) && self.shared_removal_local_source_matches(card,&source)=>{
                self.shared_views.removal_sources.insert(card,source);true
            }
            None if matches!(self.capture_shared_removal_source(card),Ok(None))=>true,
            _=>{self.status=Some(TransientNotice::warning("The shared pane changed while Close was pending. Its current source was preserved."));false}
        }
    }
    pub(in crate::app) fn prepare_shared_card_removal(&mut self,card:CardId)->bool {
        if let Some(source)=self.shared_views.removal_sources.get(&card) {
            if source.current(&self.workspace_model) && self.shared_removal_local_source_matches(card,source){return true;}
            self.shared_views.removal_sources.remove(&card);
            self.status=Some(TransientNotice::warning("The original shared Close source changed. Reconnect and choose Close again."));return false;
        }
        match self.capture_shared_removal_source(card) {
            Ok(source)=>self.install_shared_removal_source(card,source),
            Err(error)=>{self.status=Some(TransientNotice::warning(error));false}
        }
    }
}
