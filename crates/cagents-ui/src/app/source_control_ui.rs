//! Applying source control results to the bound worktree: a loaded status
//! snapshot, a page of commit history, and the end of a staged mutation.
use super::*;
use std::ops::ControlFlow;

impl Cagents {
    pub(super) fn source_control_loaded(
        &mut self,
        key: ActiveWorktreeKey,
        request: u64,
        result: Result<Box<RepositorySnapshot>, String>,
    ) -> Handled {
        if request != self.refresh_ui.source_generation || !self.source_control_is_bound(&key) {
            return ControlFlow::Break(Task::none());
        }
        self.refresh_ui.source_cancel = None;
        if let Ok(snapshot) = &result
            && let Some(worktree) = self
                .sidebar_model
                .projects
                .iter_mut()
                .find(|project| project.id == key.project_id)
                .and_then(|project| {
                    project
                        .worktrees
                        .iter_mut()
                        .find(|worktree| worktree.id == key.worktree_id)
                })
        {
            worktree.apply_status(&snapshot.status);
        }
        let mut next = match &result {
            Ok(snapshot) => SourceControlModel::ready(key, snapshot),
            Err(error) => SourceControlModel::failed(key, error.clone()),
        };
        // The reload replaces the rows and nothing else: an in-flight
        // mutation, its last failure, and the history already walked
        // belong to the binding, not to the page of Git output.
        if let Some(previous) = self.sidebar_model.source_control.as_ref() {
            previous.carry_mutation_into(&mut next);
        }
        self.sidebar_model.source_control = Some(next);
        ControlFlow::Continue(match result {
            Ok(snapshot) => self.refresh_branch_changes(&snapshot),
            Err(_) => {
                self.cancel_branch_changes();
                Task::none()
            }
        })
    }

    pub(super) fn source_control_history(
        &mut self,
        generation: u64,
        key: ActiveWorktreeKey,
        page: u32,
        head: crate::repository::Oid,
        result: Result<Vec<CommitSummary>, String>,
    ) -> Handled {
        if !self.source_control_is_bound(&key) || !self.history_ui.accepts_page(generation) {
            return ControlFlow::Break(Task::none());
        }
        match result {
            Ok(commits) => {
                if let Some(source) = self.sidebar_model.source_control.as_mut() {
                    // A page that arrives out of order would append the
                    // same commits twice, so only the page the walk is
                    // waiting for is taken.
                    if source.head_oid.as_deref() == Some(head.as_str())
                        && source.next_history_page == page
                    {
                        source.append_history(&commits);
                    }
                }
            }
            Err(error) => {
                if let Some(source) = self.sidebar_model.source_control.as_mut() {
                    if source.head_oid.as_deref() == Some(head.as_str())
                        && source.next_history_page == page
                        && source.history_loading
                    {
                        source.history_failed(format!("Could not read history: {error}"));
                    }
                }
            }
        }
        ControlFlow::Continue(Task::none())
    }

    pub(super) fn source_control_mutated(
        &mut self,
        key: ActiveWorktreeKey,
        operation: uuid::Uuid,
        report: Option<String>,
        committed: bool,
    ) -> Handled {
        let Some(mutation) = self
            .source_control_mutations
            .get(&key.canonical_path)
            .copied()
            .filter(|mutation| mutation.id == operation)
        else {
            return ControlFlow::Break(Task::none());
        };
        let notice_key =
            (mutation.kind == MutationKind::Commit).then(|| NoticeKey::GitCommit(key.clone()));
        self.source_control_mutations.remove(&key.canonical_path);
        if committed && let Some(revision) = mutation.commit_revision {
            self.sidebar_state.clear_submitted_commit(&key, revision);
        }
        let mut refreshes = Vec::new();
        for (&card, runtime) in &mut self.cards {
            if let CardRuntime::Repository(runtime) = runtime
                && runtime.descriptor.worktree == key.canonical_path
            {
                refreshes.push(repository_task(runtime.refresh_after_mutation(card)));
            }
        }
        let bound = self
            .sidebar_model
            .source_control
            .as_mut()
            .filter(|source| source.key.canonical_path == key.canonical_path)
            .map(|source| {
                source.set_mutation(None);
                source.set_error(report.clone());
                source.key.clone()
            });
        if let Some(report) = report {
            let notice =
                TransientNotice::error(format!("{}: {report}", display_path(&key.canonical_path)));
            self.status = Some(match notice_key {
                Some(key) => notice.with_key(key),
                None => notice,
            });
        } else if let Some(notice_key) = notice_key {
            // Only an admitted Commit's complete success resolves its
            // report. Path selections and push targets are not captured
            // here, so other Git actions retain their unkeyed notices.
            self.resolve_notice(notice_key);
        }
        // Navigation does not cancel Git. Only this exact completion
        // releases the worktree, then the index is read afresh.
        if let Some(bound) = bound {
            refreshes.push(self.reload_source_control(&bound));
        }
        ControlFlow::Continue(Task::batch(refreshes))
    }
}
