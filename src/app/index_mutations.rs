//! Ordered Stage/Unstage requests owned by the worktree's running mutation.
use super::*;
use crate::repository::PathSelection;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    Stage,
    Unstage,
}

impl Kind {
    fn mutation(self) -> MutationKind {
        match self {
            Self::Stage => MutationKind::Stage,
            Self::Unstage => MutationKind::Unstage,
        }
    }
}

#[derive(Debug)]
struct Batch {
    kind: Kind,
    selection: Vec<PathSelection>,
    seen: HashSet<(
        crate::repository::RepoPath,
        Option<crate::repository::RepoPath>,
    )>,
}

impl Batch {
    fn new(kind: Kind, selection: Vec<PathSelection>) -> Self {
        let mut batch = Self {
            kind,
            selection: Vec::new(),
            seen: HashSet::new(),
        };
        batch.extend(selection);
        batch
    }

    fn extend(&mut self, selection: Vec<PathSelection>) {
        for selected in selection {
            // Keep rename sources: two observations of one destination can
            // still name different original paths that Git must consider.
            if self
                .seen
                .insert((selected.path.clone(), selected.old_path.clone()))
            {
                self.selection.push(selected);
            }
        }
    }
}

#[derive(Debug, Default)]
pub(super) struct Pending {
    batches: VecDeque<Batch>,
    count: usize,
}

impl Pending {
    fn push(&mut self, kind: Kind, selection: Vec<PathSelection>) {
        if let Some(tail) = self.batches.back_mut()
            && tail.kind == kind
        {
            let previous = tail.selection.len();
            tail.extend(selection);
            self.count += tail.selection.len() - previous;
        } else {
            let batch = Batch::new(kind, selection);
            self.count += batch.selection.len();
            self.batches.push_back(batch);
        }
    }

    pub(super) fn len(&self) -> usize {
        self.count
    }

    fn pop(&mut self) -> Option<Batch> {
        let batch = self.batches.pop_front()?;
        self.count -= batch.selection.len();
        Some(batch)
    }
}

impl Cagents {
    pub(super) fn queue_index_mutation(
        &mut self,
        key: ActiveWorktreeKey,
        kind: Kind,
        selection: Vec<PathSelection>,
    ) -> Task<Message> {
        if selection.is_empty()
            || !self.source_control_is_bound(&key)
            || self.folder_scope(&key)
            || !self
                .sidebar_model
                .source_control
                .as_ref()
                .is_some_and(|source| matches!(source.data_state, sidebar::SidebarDataState::Ready))
        {
            return Task::none();
        }
        if let Some(running) = self.source_control_mutations.get_mut(&key.canonical_path) {
            if running.key == key {
                running.pending.push(kind, selection);
                if let Some(source) = self.sidebar_model.source_control.as_mut() {
                    source.queued_index_changes = running.pending.len();
                }
            }
            return Task::none();
        }
        let Some(operation) = self.begin_mutation(&key, kind.mutation(), None) else {
            return Task::none();
        };
        Self::index_mutation_task(key, operation, Batch::new(kind, selection))
    }

    fn index_mutation_task(
        key: ActiveWorktreeKey,
        operation: uuid::Uuid,
        batch: Batch,
    ) -> Task<Message> {
        Self::mutation_task(key.clone(), operation, move |service, cancellation| {
            let report = match batch.kind {
                Kind::Stage => mutation_report(
                    "Stage",
                    service.stage(
                        &StageRequest {
                            worktree: key.canonical_path,
                            selection: batch.selection,
                        },
                        cancellation,
                    ),
                ),
                Kind::Unstage => mutation_report(
                    "Unstage",
                    service.unstage(
                        &UnstageRequest {
                            worktree: key.canonical_path,
                            selection: batch.selection,
                        },
                        cancellation,
                    ),
                ),
            };
            (report, false)
        })
    }

    pub(super) fn finish_source_control_mutation(
        &mut self,
        key: ActiveWorktreeKey,
        operation: uuid::Uuid,
        mut report: Option<String>,
        committed: bool,
    ) -> Task<Message> {
        if !self
            .source_control_mutations
            .get(&key.canonical_path)
            .is_some_and(|running| running.id == operation && running.key == key)
        {
            return Task::none();
        }
        let mut running = self
            .source_control_mutations
            .remove(&key.canonical_path)
            .expect("the exact mutation was just verified");
        if committed && let Some(revision) = running.commit_revision {
            self.sidebar_state.clear_submitted_commit(&key, revision);
        }
        let pending = running.pending.len();
        if pending > 0 && !self.sidebar_model.contains_worktree(&key) {
            report.get_or_insert_with(|| "The original worktree is no longer available".into());
        }
        if report.is_none()
            && let Some(next) = running.pending.pop()
        {
            // The retained owner, rather than whichever panel is focused,
            // supplies both the destination and the next ordered request.
            let operation = uuid::Uuid::new_v4();
            running.id = operation;
            running.kind = next.kind.mutation();
            running.commit_revision = None;
            if let Some(source) = self
                .sidebar_model
                .source_control
                .as_mut()
                .filter(|source| source.key == key)
            {
                source.set_mutation(Some(running.kind));
                source.queued_index_changes = running.pending.len();
            }
            self.source_control_mutations
                .insert(key.canonical_path.clone(), running);
            return Self::index_mutation_task(key, operation, next);
        }
        if pending > 0
            && let Some(failure) = report.as_mut()
        {
            failure.push_str(&format!(
                " {pending} queued file actions were not run. Review the changes and try again."
            ));
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
                source.queued_index_changes = 0;
                source.index_queue_blocked = false;
                if source.key == key {
                    source.set_error(report.clone());
                }
                source.key.clone()
            });
        if let Some(report) = report {
            self.status = Some(TransientNotice::error(format!(
                "{}: {report}",
                display_path(&key.canonical_path)
            )));
        }
        // All accepted batches have settled, or a failure stopped them with
        // an explicit report. Reconcile once, including a partial Git effect.
        if let Some(bound) = bound {
            refreshes.push(self.reload_source_control(&bound));
        }
        Task::batch(refreshes)
    }
}

#[cfg(test)]
mod tests;
