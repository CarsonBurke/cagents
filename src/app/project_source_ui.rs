//! The window accepts phase one before any enrollment, then checks phase two
//! again before the existing repository publication/deferred navigation path.
use super::*;
use crate::edited_root_admission::{AdmissionPhase, AdmissionTicket};
use project_source_admission::RegisteredProjectSource;

impl Cagents {
    /// Install only the worker-built identity for this same published checkout.
    /// Linked roots have their own admission path; a project-primary lease must
    /// never substitute for them. This performs no filesystem or client calls.
    pub(super) fn install_published_repository_editor_root(
        &self,
        runtime: &mut RepositoryRuntime,
    ) -> Result<(), String> {
        let Some(registered) = self.retained_registered_project_source(runtime.descriptor.project)
        else { return Ok(()); };
        if registered.source.repository() != runtime.descriptor.worktree {
            return Ok(());
        }
        runtime.set_original_editor_root(registered.editor_root).map_err(|_| {
            "The original project root is retiring or already installed. Reopen this pane after the project reloads.".into()
        })
    }

    pub(super) fn registered_project_source_is_current(registered: &RegisteredProjectSource) -> bool {
        let Ok(admitted) = registered.source.edited_root_admission() else { return false; };
        let Some(editor_admitted) = registered.editor_root.retained_admission() else { return false; };
        registered.source.is_available() && admitted.ticket().phase() == AdmissionPhase::Active
            && editor_admitted.ticket().id() == admitted.ticket().id()
            && Arc::ptr_eq(&editor_admitted.lease(), &admitted.lease())
            && Arc::ptr_eq(&registered.editor_root.retained_root(), &registered.source.retained_worktree_root())
    }

    /// Every object here came from the worker, including its prebuilt editor
    /// identity. This check clones cached handles and reads no filesystem state.
    pub(super) fn checked_repository_publication(&self, result: Result<Box<LoadedRepository>, String>)
        -> Result<Box<LoadedRepository>, String> {
        let loaded = result?;
        let valid = match (&loaded.creation_source, &loaded.registered_creation) {
            (Some(source), Some(registered)) => source.id() == registered.source.id()
                && Self::registered_project_source_is_current(registered)
                && self.exit.is_none()
                && (!self.update_prepared || self.retained_project_creation_source(loaded.project.id)
                    .is_some_and(|published| published.id() == source.id())),
            (None, None) => loaded.project.kind == ProjectKind::Folder
                || loaded.repository.as_ref().is_some_and(|repository| repository.is_bare)
                || !crate::repository::WorktreeAuthority::supported(),
            _ => false,
        };
        valid.then_some(loaded).ok_or_else(|| "The original project root is not enrolled for this window. Reload the project before opening files or creating a worktree.".into())
    }

    fn repository_preparation_is_current(&self, project: ProjectId, generation: u64,
        loaded: &LoadedRepository) -> bool {
        loaded.project.id == project && self.exit.is_none() && !crate::device::is_remote()
            && self.repository_generations.get(&project) == Some(&generation)
            && self.refresh_ui.projects.get(&project).is_some_and(|cancel| !cancel.is_cancelled())
            && !self.project_ui.closed.contains(&loaded.project.repository)
            && !self.edited_root_admissions.lock().unwrap_or_else(std::sync::PoisonError::into_inner).is_sealed()
    }

    pub(super) fn finish_repository_preparation(&mut self, project: ProjectId, generation: u64,
        open_cards: bool, result: Result<Box<PreparedProjectRepository>, ProjectPreparationFailure>) -> Task<Message> {
        let prepared = match result {
            Err(error) => {
                // The original window ledger already owns every prepared or
                // uncertain resource before this callback or its stale filter.
                if let Some(ticket) = error.ticket() { ticket.request_retirement(); }
                return self.update(Message::RepositoryLoaded { project_id: project, generation, open_cards,
                    result: Err(error.to_string()) });
            }
            Ok(prepared) => *prepared,
        };
        let PreparedProjectRepository { mut loaded, prepared_creation, editor_root } = prepared;
        if !self.repository_preparation_is_current(project, generation, &loaded)
            || (self.update_prepared && (prepared_creation.is_some() || open_cards)) {
            if let Some(creation) = prepared_creation { creation.ticket().request_retirement(); }
            if self.repository_generations.get(&project) != Some(&generation) { return Task::none(); }
            return self.update(Message::RepositoryLoaded { project_id: project, generation, open_cards: false,
                result: Err("Opening this project was paused or cancelled. Reload it to try again; original root custody is retained until retirement.".into()) });
        }
        let Some(creation) = prepared_creation else {
            loaded.registered_creation = match (loaded.creation_source.clone(), editor_root) {
                (Some(source), Some(editor_root)) => Some(RegisteredProjectSource { source, editor_root }),
                _ => None,
            };
            return self.update(Message::RepositoryLoaded { project_id: project, generation, open_cards,
                result: self.checked_repository_publication(Ok(Box::new(loaded))) });
        };
        let ticket = creation.ticket().clone();
        let worker_ticket = ticket.clone();
        let cancellation = self.refresh_ui.projects[&project].clone();
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || {
                let registered = creation.enroll(&cancellation, Instant::now() + Duration::from_secs(30))?;
                loaded.creation_source = Some(registered.source.clone());
                loaded.registered_creation = Some(registered);
                Ok(Box::new(loaded))
            }).await.map_err(|error| ProjectPreparationFailure::with_ticket(worker_ticket,
                format!("Original project enrollment worker interrupted: {error}"))).and_then(|result| result);
            Message::RepositoryAdmitted { project_id: project, generation, open_cards, ticket, result }
        })
    }

    pub(super) fn finish_repository_admission(&mut self, project: ProjectId, generation: u64,
        open_cards: bool, ticket: AdmissionTicket, result: Result<Box<LoadedRepository>, ProjectPreparationFailure>) -> Task<Message> {
        // Ticket state is recorded by the worker before Message delivery. A
        // discarded result drops only its passive owner, never original custody.
        let result = match result {
            Ok(loaded) if !self.update_prepared && ticket.phase() == AdmissionPhase::Active
                && loaded.registered_creation.as_ref().is_some_and(|registered|
                    registered.source.edited_root_admission().is_ok_and(|admitted| admitted.ticket().id() == ticket.id()))
                && self.repository_preparation_is_current(project, generation, &loaded) => Ok(loaded),
            Ok(_) => {
                ticket.request_retirement();
                Err("The project changed while its original root was enrolling. Reload the project to try again.".into())
            }
            Err(error) => {
                ticket.request_retirement();
                if let Some(original) = error.ticket() { original.request_retirement(); }
                Err(error.to_string())
            }
        };
        self.update(Message::RepositoryLoaded { project_id: project, generation, open_cards, result })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder_prepared(app: &mut Cagents, project: ProjectId, path: &Path) -> Box<PreparedProjectRepository> {
        let cancellation = CancellationToken::new();
        app.repository_generations.insert(project, 1);
        app.refresh_ui.projects.insert(project, cancellation.clone());
        let ticket = app.edited_root_admissions.lock().unwrap().reserve().unwrap();
        let work = ticket.begin_preparation().unwrap();
        Box::new(project_source_admission::prepare_project_repository(path, project, cancellation,
            Some(ProjectKind::Folder), None, Err("No endpoint needed for readonly folder metadata".into()),
            work, Instant::now() + Duration::from_secs(5)).unwrap())
    }

    #[test]
    fn folder_metadata_finishes_without_creating_a_registered_mutation_actor() {
        let folder = tempfile::tempdir().unwrap();
        let mut app = super::super::tests::context_inspector_test_app(false);
        let project = ProjectId::new();
        let prepared = folder_prepared(&mut app, project, folder.path());
        drop(app.update(Message::RepositoryPrepared { project_id: project, generation: 1, open_cards: false, result: Ok(prepared) }));
        assert_eq!(app.sidebar_model.project(project).unwrap().kind, ProjectKind::Folder);
        assert!(app.retained_project_creation_source(project).is_none());
        assert!(app.retained_registered_project_source(project).is_none());
        drop(app.update(Message::RootAdmissions(root_admission_ui::Action::Poll)));
        assert!(app.edited_root_admissions.lock().unwrap().is_empty());
    }

    #[test]
    fn stale_or_committed_exit_preparation_cannot_publish_a_project() {
        for sealed in [false, true] {
            let folder = tempfile::tempdir().unwrap();
            let mut app = super::super::tests::context_inspector_test_app(false);
            let project = ProjectId::new();
            let prepared = folder_prepared(&mut app, project, folder.path());
            if sealed { app.edited_root_admissions.lock().unwrap().seal_new_admissions(); }
            else { app.repository_generations.insert(project, 2); }
            drop(app.update(Message::RepositoryPrepared { project_id: project, generation: 1, open_cards: true, result: Ok(prepared) }));
            assert!(app.sidebar_model.project(project).is_none());
            assert!(app.retained_registered_project_source(project).is_none());
            assert!(app.edited_root_admissions.lock().unwrap().is_settled());
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
#[path = "project_source_contracts.rs"]
mod contracts;
