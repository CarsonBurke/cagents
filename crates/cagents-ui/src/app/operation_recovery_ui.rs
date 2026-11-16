use super::*;

impl Cagents {
    pub(super) fn retain_operation_recovery(
        &mut self,
        scope: Option<PathBuf>,
        authority: crate::recovery_authority::RecoveryAuthority,
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
    fn recovery_checkout_census(&self) -> Result<Vec<PathBuf>, String> {
        let mut paths = std::collections::BTreeSet::new();
        for project in &self.sidebar_model.projects {
            paths.insert(project.repository.clone());
            for worktree in &project.worktrees {
                paths.insert(worktree.path.clone());
            }
        }
        for runtime in self.cards.values() {
            let runtime = match runtime {
                CardRuntime::Repository(runtime) => runtime,
                CardRuntime::PullRequests(pane) => &pane.runtime,
                _ => continue,
            };
            paths.insert(runtime.descriptor.worktree.clone());
            if let Some(root) = runtime.retained_editor_root() {
                paths.insert(root.path().to_owned());
            }
        }
        if paths.len() > 256 {
            return Err(
                "Too many known checkout roots to safely place file recovery; no save was started."
                    .into(),
            );
        }
        Ok(paths.into_iter().collect())
    }
    pub(super) fn prepare_editor_save(&mut self, card: CardId) -> bool {
        let paths = match self.recovery_checkout_census() {
            Ok(paths) => paths,
            Err(error) => {
                self.status = Some(TransientNotice::error(error));
                return false;
            }
        };
        match self.cards.get_mut(&card) {
            Some(CardRuntime::Repository(runtime)) => {
                runtime.configure_save_recovery(self.operation_recovery.clone(), paths)
            }
            _ => false,
        }
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
