//! Window ownership of recovery survives tab retirement and late worker output.
//! Scope spellings refuse new UI intentions only; producer-retained descriptors
//! remain the sole authority for retry. No pathname is reopened here.
use super::*;
use cagents::recovery_authority::RecoveryAuthority;

pub(super) struct State {
    authority: RecoveryAuthority,
    scopes: BTreeSet<PathBuf>,
    unknown_scope: bool,
    pending: Option<RecoveryAuthority>,
    message: String,
}

#[derive(Debug, Clone)]
pub enum Action {
    Retry(RecoveryAuthority),
    Resolved { authority: RecoveryAuthority, result: Result<(), String> },
}

impl Editor {
    /// Actual producers call this before filtering completion by tab/document
    /// identity. The window then owns the exact opaque capability even if its
    /// original tab no longer exists. A missing scope conservatively refuses
    /// new disk work throughout the window, while reads and edits remain usable.
    pub(super) fn retain_operation_recovery(&mut self, worktree: Option<PathBuf>, authority: RecoveryAuthority, message: String) {
        if authority.is_resolved() { return; }
        // A state whose leaves resolved through another owner needs no further
        // scope attribution. Its already-running receipt will be fenced by Arc.
        if self.recovery.as_ref().is_some_and(|state| state.authority.is_resolved()) { self.recovery = None; }
        let state = self.recovery.get_or_insert_with(|| State {
            authority: authority.clone(), scopes: BTreeSet::new(), unknown_scope: false,
            pending: None, message: String::new(),
        });
        state.authority = state.authority.clone().combine(authority);
        state.message = message.chars().take(4096).collect();
        match worktree {
            Some(worktree) if state.scopes.len() < 32 || state.scopes.contains(&worktree) => { state.scopes.insert(worktree); },
            _ => state.unknown_scope = true,
        }
    }

    pub(super) fn retain_repository_recovery(&mut self, action: &RepositoryAction) {
        let (identity, authority) = match action {
            RepositoryAction::SaveFinished { identity, result, .. } => (identity, match result {
                Ok(cagents::editor::SaveOutcome::Saved { durability, .. } | cagents::editor::SaveOutcome::Absent { durability, .. }) => durability.recovery_authority(),
                Err(error) => error.recovery_authority(),
                _ => None,
            }),
            RepositoryAction::DiskLoaded { identity, result: Err(error), .. } => (identity, error.recovery_authority()),
            _ => return,
        };
        if let Some(authority) = authority {
            self.retain_operation_recovery(Some(identity.worktree.clone()), authority.clone(),
                format!("A file operation still needs recovery. {}", authority.description()));
        }
    }

    pub(super) fn editor_recovery_blocks(&self, worktree: &Path) -> bool {
        self.recovery.as_ref().is_some_and(|state| !state.authority.is_resolved()
            && (state.unknown_scope || state.scopes.contains(worktree)))
            || self.tabs.iter().any(|tab| tab.runtime.descriptor.worktree == worktree && tab.runtime.has_editor_recovery())
    }

    pub(super) fn editor_recovery_blocks_path(&self, path: &Path) -> bool {
        self.recovery.as_ref().is_some_and(|state| !state.authority.is_resolved()
            && (state.unknown_scope || state.scopes.iter().any(|scope| path.starts_with(scope))))
            || self.tabs.iter().any(|tab| path.starts_with(&tab.runtime.descriptor.worktree) && tab.runtime.has_editor_recovery())
    }

    pub(super) fn refuse_recovery_disk_action(&mut self, action: &RepositoryAction) -> bool {
        if !matches!(action, RepositoryAction::DiskMetadata { .. } | RepositoryAction::Save { .. } | RepositoryAction::AutoSaveDue { .. }
            | RepositoryAction::StageSelected { .. } | RepositoryAction::RevertFile { .. }
            | RepositoryAction::Reload { .. } | RepositoryAction::KeepMine { .. }) { return false; }
        let card = action.card();
        if !self.tab(card).is_some_and(|tab| self.editor_recovery_blocks(&tab.runtime.descriptor.worktree)) { return false; }
        if let Some(tab) = self.tab_mut(card) {
            match action {
                RepositoryAction::AutoSaveDue { identity, revision, ticket } => { tab.runtime.retire_auto_save_due(identity, *revision, *ticket); },
                RepositoryAction::DiskMetadata { identity, ticket, .. } => { tab.runtime.retire_disk_metadata(identity, *ticket); },
                _ => {},
            }
        }
        self.notice = Some("Resolve retained file recovery before another disk operation in this worktree.".into());
        true
    }

    pub(super) fn refuse_recovery_close(&mut self) -> bool {
        let pending = self.recovery.as_ref().is_some_and(|state| !state.authority.is_resolved())
            || self.tabs.iter().any(|tab| tab.runtime.has_editor_recovery());
        if pending {
            self.exiting = false;
            self.notice = Some("Resolve retained file recovery before closing this window.".into());
        }
        pending
    }

    pub(super) fn recovery_action(&mut self, action: Action) -> Task<Message> {
        match action {
            Action::Retry(authority) => {
                let Some(state) = self.recovery.as_mut().filter(|state| state.authority == authority && state.pending.is_none()) else { return Task::none(); };
                state.pending = Some(authority.clone());
                cosmic::task::future(async move {
                    let worker = authority.clone();
                    let result = tokio::task::spawn_blocking(move || RepositoryService::system("git")
                        .resolve_recovery(&worker, &CancellationToken::default()).map_err(|error| error.to_string()))
                        .await.map_err(|error| error.to_string()).and_then(|result| result);
                    Message::Recovery(Action::Resolved { authority, result })
                })
            }
            Action::Resolved { authority, result } => {
                let Some(state) = self.recovery.as_mut().filter(|state| state.pending.as_ref() == Some(&authority)) else { return Task::none(); };
                state.pending = None;
                match result {
                    Ok(()) if authority.is_resolved() && state.authority.is_resolved() => self.recovery = None,
                    Ok(()) => state.message = "An earlier operation recovered. Another retained operation still needs recovery.".into(),
                    Err(error) => state.message = format!("File recovery is still pending; retained files were kept. {error}").chars().take(4096).collect(),
                }
                self.resume_recovery_work()
            }
        }
    }

    pub(super) fn resume_recovery_work(&mut self) -> Task<Message> {
        let ids = self.tabs.iter().filter(|tab| !self.editor_recovery_blocks(&tab.runtime.descriptor.worktree)).map(|tab| tab.id).collect::<Vec<_>>();
        // Timers rejected during recovery were consumed exactly. Resume only
        // current documents whose real Runtime admits autosave. Either the
        // common retry or a Runtime's own control can resolve shared leaves.
        let mut tasks = ids.into_iter().filter_map(|id| self.tab_mut(id).map(|tab| repository_task(tab.runtime.auto_save_task(id)))).collect::<Vec<_>>();
        tasks.push(self.resume_pending_files());
        tasks.push(self.resume_change_groups());
        Task::batch(tasks)
    }

    pub(super) fn editor_recovery_view(&self) -> Option<Element<'_, Message>> {
        let state = self.recovery.as_ref().filter(|state| !state.authority.is_resolved())?;
        Some(widget::container(widget::row::with_children(vec![
            design::body(&state.message).width(Length::Fill).into(),
            cagents::ui_controls::button::secondary("Retry file recovery")
                .on_press_maybe(state.pending.is_none().then(|| Message::Recovery(Action::Retry(state.authority.clone())))).into(),
        ]).spacing(design::space::S)).padding(design::space::M).width(Length::Fill).into())
    }
}
