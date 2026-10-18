use super::*;

impl Editor {
    fn retain_operation_recovery(
        &mut self,
        scope: Option<PathBuf>,
        authority: cagents::recovery_authority::RecoveryAuthority,
    ) {
        self.operation_recovery
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(scope, authority);
    }
    pub(super) fn operation_recovery_pending(&self) -> bool {
        self.operation_recovery
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_pending()
    }
    pub(super) fn prepare_editor_save(&mut self, card: CardId) -> bool {
        let mut paths = BTreeSet::new();
        for tab in &self.tabs {
            paths.insert(tab.runtime.descriptor.worktree.clone());
            if let Some(root) = tab.runtime.retained_editor_root() {
                paths.insert(root.path().to_owned());
            }
            for project in &tab.sidebar.projects {
                paths.insert(project.repository.clone());
                for worktree in &project.worktrees {
                    paths.insert(worktree.path.clone());
                }
            }
        }
        if paths.len() > 256 {
            self.notice = Some(
                "Too many known checkout roots to safely place file recovery; no save was started."
                    .into(),
            );
            return false;
        }
        let window = self.operation_recovery.clone();
        self.tab_mut(card).is_some_and(|tab| {
            tab.runtime
                .configure_save_recovery(window, paths.into_iter().collect())
        })
    }
    pub(super) fn retain_repository_operation(&mut self, action: &RepositoryAction) {
        if let Some(authority) = action.recovery_authority() {
            self.retain_operation_recovery(
                action.recovery_scope().map(Path::to_owned),
                authority.clone(),
            );
        }
    }
}
