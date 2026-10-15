//! Immutable UI consent basis. No process cleanup authority is granted here.
use super::*;

#[derive(Debug, Clone)]
pub(in crate::app) struct RemovalSource {
    pub(super) registry: uuid::Uuid,
    pub(super) repository: PathBuf,
    pub(super) scope: Scope,
    pub(super) before: Pane,
    execution_instance: Option<uuid::Uuid>,
    observed_chat: Option<crate::chat::Session>,
    handoff: Option<crate::terminal::ipc::TerminalHandoffReceipt>,
}
impl RemovalSource {
    pub(super) fn change(&self) -> Change {
        Change::RemoveSource {
            registry: self.registry,
            repository: self.repository.clone(),
            before: self.before.clone(),
            execution_instance: self.execution_instance,
         handoff: self.handoff.clone(), }
    }
    /// Called only after the exact Target-owned successful host return and
    /// descriptor transition. Keeps the original issued wire pane and filing.
    pub(in crate::app) fn after_terminal_handoff(mut self,receipt:&crate::terminal::ipc::TerminalHandoffReceipt,observed_after:Option<&crate::chat::Session>)->Option<Self> {
        if self.handoff.is_some() || !views::handoff_source_matches(&self.before,receipt)
            || !self.observed_chat.as_ref().is_some_and(|observed|super::super::terminal_close::same_chat_removal_source(observed,&receipt.source)) {return None;}
        if let Some(after)=observed_after {
            let mut expected=receipt.source.clone();
            expected.terminal=Some(receipt.terminal.session.clone());
            if !super::super::terminal_close::same_chat_removal_source(&expected,after) {return None;}
        }
        self.execution_instance=Some(receipt.terminal.execution_instance);
        self.observed_chat=observed_after.cloned();
        self.handoff=Some(receipt.clone());
        Some(self)
    }
    pub(in crate::app) fn matches_handoff(&self,receipt:&crate::terminal::ipc::TerminalHandoffReceipt)->bool {
        self.handoff.as_ref()==Some(receipt) && self.execution_instance==Some(receipt.terminal.execution_instance)
    }
    /// Project only an authenticated, source-owned parked actor cleanup.
    /// The issued wire pane and all worker/registry authority stay unchanged.
    pub(in crate::app) fn after_observed_chat_cleanup(mut self,closed:&crate::chat::Session)->Option<Self> {
        let expected=self.observed_chat.as_ref()?;
        let pair=closed.terminal.as_ref()?;
        if !super::super::terminal_close::same_chat_removal_source(expected,closed)
            || !views::same_card_source(&CardKind::Terminal(pair.clone()),&self.local_kind()) {
            return None;
        }
        self.observed_chat=None;
        Some(self)
    }
    fn local_kind(&self)->CardKind {
        self.handoff.as_ref().map(|receipt|CardKind::Terminal(receipt.terminal.session.clone())).unwrap_or_else(||self.before.kind.clone())
    }
    pub(in crate::app) fn current(&self, model: &crate::workspace_model::WorkspaceModel) -> bool {
        removal_current(model, &self.scope, &self.change())
    }
}

pub(in crate::app) fn removal_current(
    model: &crate::workspace_model::WorkspaceModel,
    scope: &Scope,
    change: &Change,
) -> bool {
    let Change::RemoveSource {
        registry,
        repository,
        before,
        ..
    } = change
    else {
        return !matches!(change, Change::Remove { .. });
    };
    if *registry == crate::workspace_model::NO_REGISTRY || model.identity != *registry {
        return false;
    }
    let owner = match scope {
        Scope::Channel(id) => model.channel(*id).map(|channel| &channel.repository),
        Scope::Worktree { repository, .. } => Some(repository),
    };
    if owner != Some(repository)
        || !model
            .projects
            .iter()
            .any(|project| &project.repository == repository)
    {
        return false;
    }
    let mut matches = model.views.iter().flat_map(|view| {
        view.panes
            .iter()
            .filter(move |pane| pane.id == before.id)
            .map(move |pane| (&view.scope, pane))
    });
    let Some((current_scope, current)) = matches.next() else {
        return false;
    };
    matches.next().is_none() && current_scope == scope && views::same_removal_pane_source(before, current, match change { Change::RemoveSource {handoff,..}=>handoff.as_ref(),_=>None })
}

impl Cagents {
    /// Capture before asynchronous cleanup. None means an unpublished local
    /// pane; ambiguous or previously shared membership is an actionable refusal.
    pub(in crate::app) fn capture_shared_removal_source(
        &self,
        card: CardId,
    ) -> Result<Option<RemovalSource>, String> {
        let model = &self.workspace_model;
        let mut owners = model.views.iter().flat_map(|view| {
            view.panes
                .iter()
                .filter(move |pane| pane.id == card)
                .map(move |pane| (&view.scope, pane))
        });
        let first = owners.next();
        if owners.next().is_some() {
            return Err(
                "The pane has ambiguous shared membership; reconnect before closing it.".into(),
            );
        }
        let Some((scope, before)) = first else {
            return if self.shared_views.attached.contains(&card)
                || self
                    .shared_views
                    .baseline
                    .values()
                    .any(|panes| panes.iter().any(|pane| pane.id == card))
            {
                Err("The pane's shared source is unavailable; reconnect before closing it.".into())
            } else {
                Ok(None)
            };
        };
        let kind = self
            .card_state(card)
            .map(|card| &card.kind)
            .ok_or("The pane descriptor is unavailable.")?;
        if !super::super::terminal_close::same_removal_source(
            &before.kind,
            &views::shared_kind(kind),
        ) || before.pending_terminal_start
        {
            return Err(
                "The shared pane changed before Close; its current source was kept.".into(),
            );
        }
        let repository = match scope {
            Scope::Channel(id) => model
                .channel(*id)
                .map(|channel| channel.repository.clone())
                .ok_or("The pane's project is unavailable.")?,
            Scope::Worktree { repository, .. } => repository.clone(),
        };
        let source = RemovalSource {
            registry: model.identity,
            repository,
            scope: scope.clone(),
            before: before.clone(),
            execution_instance: self.terminal_worker_for_shared_removal(card),
            observed_chat: self.chat_return_session(card),
            handoff: None,
        };
        if !source.current(model) {
            return Err("The shared pane source is no longer authoritative.".into());
        }
        Ok(Some(source))
    }
    fn shared_removal_local_source_matches(&self, card: CardId, source: &RemovalSource) -> bool {
        self.card_state(card).is_some_and(|state| {
            super::super::terminal_close::same_removal_source(
                &source.local_kind(),
                &views::shared_kind(&state.kind),
            )
        }) && match (&source.observed_chat, self.chat_return_session(card)) {
            (Some(expected), Some(current)) => {
                super::super::terminal_close::same_chat_removal_source(expected, &current)
            }
            (None, None) => true,
            _ => false,
        }
    }
    /// Install only the original authenticated basis; never select a replacement
    /// source after a lifecycle reply. Local drafts need no shared capability.
    pub(in crate::app) fn install_shared_removal_source(
        &mut self,
        card: CardId,
        source: Option<RemovalSource>,
    ) -> bool {
        match source {
            Some(source)
                if source.before.id == card
                    && source.current(&self.workspace_model)
                    && self.shared_removal_local_source_matches(card, &source) =>
            {
                self.shared_views.removal_sources.insert(card, source);
                true
            }
            None if matches!(self.capture_shared_removal_source(card), Ok(None)) => true,
            _ => {
                self.status = Some(TransientNotice::warning(
                    "The shared pane changed while Close was pending. Its current source was preserved.",
                ));
                false
            }
        }
    }
    pub(in crate::app) fn prepare_shared_card_removal(&mut self, card: CardId) -> bool {
        if let Some(source) = self.shared_views.removal_sources.get(&card) {
            if source.current(&self.workspace_model)
                && self.shared_removal_local_source_matches(card, source)
            {
                return true;
            }
            self.shared_views.removal_sources.remove(&card);
            self.status = Some(TransientNotice::warning(
                "The original shared Close source changed. Reconnect and choose Close again.",
            ));
            return false;
        }
        match self.capture_shared_removal_source(card) {
            Ok(source) => self.install_shared_removal_source(card, source),
            Err(error) => {
                self.status = Some(TransientNotice::warning(error));
                false
            }
        }
    }
}

#[cfg(test)]
mod cleanup_projection_tests {
    use super::*;
    #[test]
    fn successful_parked_cleanup_changes_only_captured_observation_not_wire_source() {
        let launch=LaunchRecipe {program:"fixture".into(),arguments:vec![],working_directory:"/repo".into(),environment:Default::default(),unset_environment:Default::default()};
        let terminal=TerminalSession {id:TerminalId::new(),launch:launch.clone(),transcript_path:None,resume_hint:None,owned_codex_listener:None,recovery_origin:None,predecessor:None,continuity:TerminalContinuity::RelaunchAvailable {recipe:launch.clone(),last_exit:None}};
        let closed=crate::chat::Session {id:TerminalId::new(),launch:crate::chat::Launch {provider:crate::agent::Provider::Claude,recipe:launch,session_id:Some("original".into())},proof:None,terminal:Some(terminal.clone()),owned_codex_listener:None,native_continuity:Default::default()};
        let source=RemovalSource {registry:uuid::Uuid::new_v4(),repository:"/repo".into(),scope:Scope::Worktree {repository:"/repo".into(),path:"/repo".into()},before:Pane {id:CardId::new(),title:"CLI".into(),user_title:None,pending_terminal_start:false,width_fraction:1.0,kind:CardKind::Terminal(terminal),scope:None},execution_instance:Some(uuid::Uuid::new_v4()),observed_chat:Some(closed.clone()),handoff:None};
        let wire=source.change();let projected=source.clone().after_observed_chat_cleanup(&closed).unwrap();
        assert!(projected.observed_chat.is_none());assert_eq!(projected.change(),wire);
        let mut replacement=closed.clone();replacement.launch.session_id=Some("replacement".into());assert!(source.clone().after_observed_chat_cleanup(&replacement).is_none());
        replacement=closed.clone();replacement.terminal.as_mut().unwrap().id=TerminalId::new();assert!(source.clone().after_observed_chat_cleanup(&replacement).is_none());
        replacement=closed;replacement.terminal=None;assert!(source.after_observed_chat_cleanup(&replacement).is_none());
    }
}

#[cfg(test)]
mod handoff_projection_tests {
    use super::*;
    #[test]
    fn handoff_and_successful_parked_cleanup_keep_original_wire_basis() {
        let (before,receipt)=views::handoff_tests::fixture();
        let original=RemovalSource {registry:uuid::Uuid::new_v4(),repository:"/repo".into(),scope:Scope::Worktree {repository:"/repo".into(),path:"/repo".into()},before:before.clone(),execution_instance:None,observed_chat:Some(receipt.source.clone()),handoff:None};
        let mut parked=receipt.source.clone();parked.terminal=Some(receipt.terminal.session.clone());
        let advanced=original.clone().after_terminal_handoff(&receipt,Some(&parked)).unwrap();
        assert_eq!(advanced.before,before);assert!(advanced.matches_handoff(&receipt));
        let completed=advanced.after_observed_chat_cleanup(&parked).unwrap();
        assert_eq!(completed.before,before);assert!(completed.observed_chat.is_none());
        assert!(matches!(completed.change(),Change::RemoveSource {before:wire,handoff:Some(issued),execution_instance:Some(worker),..} if wire==before && issued==receipt && worker==receipt.terminal.execution_instance));
        let gone=original.clone().after_terminal_handoff(&receipt,None).unwrap();assert!(gone.observed_chat.is_none());
        let mut different=receipt.clone();different.source.launch.session_id=Some("other".into());different.source_binding.source=different.source.clone();
        assert!(original.clone().after_terminal_handoff(&different,None).is_none());
        let mut foreign_pair=parked;foreign_pair.terminal.as_mut().unwrap().id=TerminalId::new();
        assert!(original.after_terminal_handoff(&receipt,Some(&foreign_pair)).is_none());
    }
}
