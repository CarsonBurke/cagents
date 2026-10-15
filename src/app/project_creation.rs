//! Project creation uses the physical source published with that project's read.
use super::*;
use super::worktree_dialog::WorktreeCreationSource;

#[derive(Debug)]
pub(super) struct PublishedSource {
    generation: u64,
    repository: PathBuf,
    source: WorktreeCreationSource,
}

impl Cagents {
    /// Logical state only: filesystem validation belongs to the worker service.
    pub(super) fn project_creation_source(&self, project: ProjectId) -> Option<WorktreeCreationSource> {
        let row = self.sidebar_model.project(project)?;
        let published = self.project_ui.creation_sources.get(&project)?;
        (row.kind == ProjectKind::Git && !row.reloading && row.unreachable.is_none()
            && row.repository == published.repository
            && row.creation_binding == Some(published.source.id())
            && self.repository_generations.get(&project) == Some(&published.generation)
            && published.source.is_available()).then(|| published.source.clone())
    }

    pub(super) fn current_project_creation_source(&self, project: ProjectId, id: OperationId) -> bool {
        self.project_creation_source(project).is_some_and(|source| source.id() == id)
    }

    /// Called before a new project read, close or removal. Retained dialog and
    /// worker clones are invalidated together, including an already running read.
    pub(super) fn retire_project_creation_source(&mut self, project: ProjectId) {
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
            if let Some(source) = source { source.invalidate(); }
            return;
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
                generation, repository: row.repository.clone(), source,
            });
        }
    }
}
