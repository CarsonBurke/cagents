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
    archived_local: Option<CardKind>,
    operation: uuid::Uuid,
}
impl RemovalSource {
    /// Set only by the initial private-readiness/canonical-source admission.
    /// This never confers provider or terminal teardown authority.
    pub(in crate::app) fn is_archived_automation(&self)->bool {self.archived_local.is_some()}
    /// Minted with the original source before local retirement/publication.
    /// This ID is a readonly lookup reference, never a readiness capability.
    pub(in crate::app) fn archived_membership_operation(&self)->Option<uuid::Uuid> {
        self.is_archived_automation().then_some(self.operation)
    }
    pub(super) fn change(&self) -> Change {
        if self.is_archived_automation() {
            return Change::RemoveArchivedAutomationSource {registry:self.registry,repository:self.repository.clone(),before:self.before.clone()};
        }
        Change::RemoveSource {
            registry: self.registry,
            repository: self.repository.clone(),
            before: self.before.clone(),
            execution_instance: self.execution_instance,
        }
    }
    /// Project only an authenticated, source-owned parked actor cleanup.
    /// The issued wire pane and all worker/registry authority stay unchanged.
    pub(in crate::app) fn after_observed_chat_cleanup(mut self,closed:&crate::chat::Session)->Option<Self> {
        let expected=self.observed_chat.as_ref()?;
        let pair=closed.terminal.as_ref()?;
        if !super::super::terminal_close::same_chat_removal_source(expected,closed)
            || !matches!(&self.before.kind,CardKind::Terminal(source) if views::same_card_source(&CardKind::Terminal(pair.clone()),&CardKind::Terminal(source.clone()))) {
            return None;
        }
        self.observed_chat=None;
        Some(self)
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
    let (Change::RemoveSource {
        registry,
        repository,
        before,
        ..
    } | Change::RemoveArchivedAutomationSource {registry,repository,before}) = change
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
    matches.next().is_none() && current_scope == scope && match change {
        Change::RemoveArchivedAutomationSource {..}=>same_archive_pane_source(before,current),
        _=>views::same_pane_source(before,current),
    }
}

fn same_archive_pane_source(expected:&Pane,current:&Pane)->bool {
    expected.id==current.id && expected.kind==current.kind && expected.scope==current.scope
        && !expected.pending_terminal_start && !current.pending_terminal_start
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
        // Read-only archive presentation may deliberately be Chat over its
        // immutable canonical console Terminal. Only the producer's exact
        // private readiness and canonical mapping admits that projection.
        let archived_local=match self.archived_pane_readiness(card) {
            Ok(Some(true)) if self.canonical_automation_archive_pane(card).as_ref()==Some(before)=>Some(kind.clone()),
            Ok(None)=>None,
            _=>return Err("The original automation archive is not ready or changed; its pane and saved result were preserved.".into()),
        };
        if (archived_local.is_none() && !super::super::terminal_close::same_removal_source(
            &before.kind,
            &views::shared_kind(kind),
        )) || before.pending_terminal_start
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
            archived_local,
            operation:uuid::Uuid::new_v4(),
        };
        if !source.current(model) {
            return Err("The shared pane source is no longer authoritative.".into());
        }
        Ok(Some(source))
    }
    fn shared_removal_local_source_matches(&self, card: CardId, source: &RemovalSource) -> bool {
        let local=if let Some(expected)=&source.archived_local {
            matches!(self.archived_pane_readiness(card),Ok(Some(true)))
                && self.canonical_automation_archive_pane(card).is_some_and(|pane|same_archive_pane_source(&source.before,&pane))
                && self.card_state(card).is_some_and(|state|&state.kind==expected)
        } else { self.card_state(card).is_some_and(|state| {
            super::super::terminal_close::same_removal_source(
                &source.before.kind,
                &views::shared_kind(&state.kind),
            )
        }) };
        local && match (&source.observed_chat, self.chat_return_session(card)) {
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
        let source=RemovalSource {registry:uuid::Uuid::new_v4(),repository:"/repo".into(),scope:Scope::Worktree {repository:"/repo".into(),path:"/repo".into()},before:Pane {id:CardId::new(),title:"CLI".into(),user_title:None,pending_terminal_start:false,width_fraction:1.0,kind:CardKind::Terminal(terminal),scope:None},execution_instance:Some(uuid::Uuid::new_v4()),observed_chat:Some(closed.clone()),archived_local:None,operation:uuid::Uuid::new_v4()};
        let wire=source.change();let projected=source.clone().after_observed_chat_cleanup(&closed).unwrap();
        assert!(projected.observed_chat.is_none());assert_eq!(projected.change(),wire);
        let mut replacement=closed.clone();replacement.launch.session_id=Some("replacement".into());assert!(source.clone().after_observed_chat_cleanup(&replacement).is_none());
        replacement=closed.clone();replacement.terminal.as_mut().unwrap().id=TerminalId::new();assert!(source.clone().after_observed_chat_cleanup(&replacement).is_none());
        replacement=closed;replacement.terminal=None;assert!(source.after_observed_chat_cleanup(&replacement).is_none());
    }
}
