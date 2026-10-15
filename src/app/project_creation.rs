//! Project creation uses the physical source published with that project's read.
use super::*;
use super::worktree_dialog::WorktreeCreationSource;

#[derive(Debug)]
pub(super) struct PublishedSource {
    read_generation: u64,
    logical_generation: u64,
    repository: PathBuf,
    source: WorktreeCreationSource,
}

impl Cagents {
    /// Consume the original displayed project token; never upgrade an old row
    /// action to the source that happens to occupy the same project today.
    pub(super) fn run_project_creation_target(&mut self, target: sidebar::commands::ProjectCreationTarget) -> Task<Message> {
        if crate::device::is_remote() || self.update_prepared || self.modal_is_open() || self.focused_repository_is_saving()
            || self.workspace_inputs_blocked() || !target.current_in(&self.sidebar_model) {
            return Task::none();
        }
        let Some(source) = self.project_creation_source(target.project_id).filter(|source| source.id() == target.binding) else {
            self.status = Some(TransientNotice::warning("Reload the project before creating a worktree"));
            return Task::none();
        };
        self.open_worktree_dialog_project_bound(target.project_id, source, WorktreeDraft {
            base: target.branch.as_deref().unwrap_or("").trim_start_matches("refs/heads/").to_owned(),
            ..Default::default()
        }, None)
    }

    /// Internal refreshes retain their already-published physical actor even
    /// while a replacement read is in flight. They never mint click authority.
    pub(super) fn retained_project_creation_source(&self, project: ProjectId) -> Option<WorktreeCreationSource> {
        let row = self.sidebar_model.project(project)?;
        let published = self.project_ui.creation_sources.get(&project)?;
        (row.repository == published.repository && row.creation_binding == Some(published.source.id())
            && published.source.is_available()).then(|| published.source.clone())
    }

    /// New user intents additionally require a completed, current project read.
    pub(super) fn project_creation_source(&self, project: ProjectId) -> Option<WorktreeCreationSource> {
        let row = self.sidebar_model.project(project)?;
        let published = self.project_ui.creation_sources.get(&project)?;
        if row.kind != ProjectKind::Git || row.reloading || row.unreachable.is_some()
            || self.repository_generations.get(&project) != Some(&published.read_generation) { return None; }
        self.retained_project_creation_source(project)
    }

    /// Already admitted effects survive an unchanged automatic refresh.
    pub(super) fn current_project_creation_source(&self, project: ProjectId, id: OperationId) -> bool {
        self.sidebar_model.project(project).is_some_and(|row| row.unreachable.is_none())
            && self.retained_project_creation_source(project).is_some_and(|source| source.id() == id)
    }

    pub(super) fn project_creation_generation(&self, project: ProjectId) -> Option<u64> {
        if self.sidebar_model.project(project)?.unreachable.is_some() { return None; }
        self.retained_project_creation_source(project)?;
        Some(self.project_ui.creation_sources.get(&project)?.logical_generation)
    }

    /// Called on actual ownership replacement, close or removal. Retained
    /// dialog and worker clones are invalidated together.
    pub(super) fn retire_project_creation_source(&mut self, project: ProjectId) {
        self.invalidate_project_worktree_creations(project);
        if let Some(published) = self.project_ui.creation_sources.remove(&project) {
            published.source.invalidate();
        }
        if let Some(row) = self.sidebar_model.projects.iter_mut().find(|row| row.id == project) {
            row.creation_binding = None;
        }
    }

    pub(super) fn publish_project_creation_source(&mut self, project: ProjectId, generation: u64,
        source: Option<WorktreeCreationSource>, bare: bool) {
        if self.repository_generations.get(&project) != Some(&generation) {
            // A stale read may hold the same actor as a newer read or accepted
            // creation. Dropping that read's clone grants no retirement right.
            return;
        }
        // A verified refresh carries the SAME source. Keep its logical
        // generation and all accepted workers; only its read counter advances.
        if let (Some(incoming), Some(published)) = (source.as_ref(), self.project_ui.creation_sources.get_mut(&project)) {
            if incoming.id() == published.source.id() && incoming.is_available() {
                if let Some(row) = self.sidebar_model.projects.iter_mut().find(|row| row.id == project && row.repository == published.repository) {
                    published.read_generation = generation;
                    row.creation_binding = Some(incoming.id());
                    row.creation_unavailable_reason = None;
                    return;
                }
            }
        }
        self.retire_project_creation_source(project);
        let Some(row) = self.sidebar_model.projects.iter_mut().find(|row| row.id == project) else {
            if let Some(source) = source { source.invalidate(); }
            return;
        };
        row.creation_unavailable_reason = if bare {
            Some(Arc::from("Worktree creation is unavailable for this bare repository."))
        } else {
            crate::repository::WorktreeAuthority::unsupported_reason().map(Arc::from)
        };
        if let Some(source) = source.filter(WorktreeCreationSource::is_available) {
            row.creation_binding = Some(source.id());
            row.creation_unavailable_reason = None;
            self.project_ui.creation_sources.insert(project, PublishedSource {
                read_generation: generation, logical_generation: generation,
                repository: row.repository.clone(), source,
            });
        }
    }
    /// Called directly at final message ingress, after any modal/save deferral.
    /// The guarded project variant never lowers to an unbound queued message.
    pub(super) fn apply_worktree_created(&mut self, project_id: ProjectId, repository: PathBuf,
        result: Result<WorktreeCreationOutcome, String>) -> Task<Message> {
            match result {
                Ok(WorktreeCreationOutcome::Created { worktree, .. }) => {
                    if let Some(session) = &self.session {
                        session.save_worktrees([WorktreeState {
                            id: worktree.id,
                            project_id,
                            generation: 0,
                            path: worktree.path.clone(),
                            branch: worktree.branch.as_deref().map(|branch| {
                                branch
                                    .strip_prefix("refs/heads/")
                                    .unwrap_or(branch)
                                    .to_owned()
                            }),
                            existence: Existence::Open,
                        }]);
                    }
                    self.status = Some(TransientNotice::success(format!(
                        "Created worktree {}",
                        worktree.path.display()
                    )));
                    if let Some(project) = self
                        .sidebar_model
                        .projects
                        .iter_mut()
                        .find(|project| project.id == project_id)
                    {
                        project
                            .worktrees
                            .push(SidebarWorktree::from_repository(&worktree, None));
                    }
                    let terminal = self.select_worktree(worktree.path, true);
                    let chrome = self.sync_chrome();
                    return Task::batch([
                        terminal,
                        chrome,
                        self.repository_load_task(repository, Some(project_id), false),
                    ]);
                }
                Ok(WorktreeCreationOutcome::NotCreated { failure }) => {
                    self.status = Some(TransientNotice::warning(format!(
                        "Could not create the worktree: {failure}"
                    )));
                }
                Ok(WorktreeCreationOutcome::OutcomeUnknown { failure, .. }) => {
                    self.status = Some(TransientNotice::warning(format!(
                        "The worktree creation outcome is unknown. Refresh the project before trying again. {failure}"
                    )));
                }
                Err(error) => self.status = Some(TransientNotice::error(error)),
            }
            Task::none()

    }
}
