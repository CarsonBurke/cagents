//! Actual asynchronous project-source admission. Phase one publishes only
//! readonly metadata and a preheld original owner; phase two enrolls that exact
//! owner and mints a Source only after physical and admission checks.
use super::*;
use crate::edited_root_admission::{AdmissionPhase, AdmissionTicket, PreparationWork, PreparedRoot};
use crate::recovery_domain::{LocalRecoveryEndpoint, RecoveryPlacementClient};
use crate::repository::WorktreeAuthority;

#[derive(Debug, Clone)]
pub struct PreparedProjectRepository {
    pub(super) loaded: LoadedRepository,
    pub(super) prepared_creation: Option<PreparedProjectCreation>,
}
#[derive(Debug, Clone)]
pub(super) struct PreparedProjectCreation { authority: WorktreeAuthority, admission: PreparedRoot }
#[derive(Debug, Clone)]
pub struct ProjectPreparationFailure { message: String, ticket: Option<AdmissionTicket> }
impl std::fmt::Display for ProjectPreparationFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str(&self.message) }
}
impl std::error::Error for ProjectPreparationFailure {}
impl ProjectPreparationFailure {
    pub(super) fn before_reservation(message: impl ToString) -> Self { Self { message: message.to_string(), ticket: None } }
    pub(super) fn with_ticket(ticket: AdmissionTicket, message: impl ToString) -> Self {
        ticket.request_retirement();
        Self { message: message.to_string(), ticket: Some(ticket) }
    }
    pub(super) fn ticket(&self) -> Option<&AdmissionTicket> { self.ticket.as_ref() }
}

impl PreparedProjectCreation {
    pub(super) fn ticket(&self) -> &AdmissionTicket { self.admission.ticket() }
    /// Blocking worker. The accepting App handler must first verify the actual
    /// logical project/read generation and window admission, then call this.
    /// Final ingress must repeat those checks plus ticket.phase()==Active.
    pub(super) fn enroll(self, cancellation: &CancellationToken, deadline: Instant)
        -> Result<worktree_dialog::WorktreeCreationSource, ProjectPreparationFailure> {
        let ticket = self.admission.ticket().clone();
        let work = self.admission.begin_enrollment().map_err(|error|
            ProjectPreparationFailure::with_ticket(ticket.clone(), error))?;
        let admitted = work.run(cancellation, deadline).map_err(|error|
            ProjectPreparationFailure::with_ticket(ticket.clone(), error))?;
        let checked = (|| -> Result<_, String> {
            let (root, _) = self.authority.recovery_roots().map_err(|error| error.to_string())?;
            admitted.validate_for_root(&root, deadline).map_err(|error| error.to_string())?;
            if cancellation.is_cancelled() || ticket.phase() != AdmissionPhase::Active {
                return Err("The original project closed during source enrollment; its custody is retained.".into());
            }
            worktree_dialog::WorktreeCreationSource::new_registered(self.authority, admitted)
                .map_err(|error| error.to_string())
        })();
        checked.map_err(|error| ProjectPreparationFailure::with_ticket(ticket, error))
    }
}

/// Worker-only phase one. Connect/Catalog bootstrap has no negative Root
/// registration effect. The client prepares a charged owner without RPC, and
/// the window's pre-reserved ticket owns it before any message or enrollment.
pub(super) fn prepare_project_repository(
    path: &Path, project_id: ProjectId, cancellation: CancellationToken,
    kind: Option<ProjectKind>, retained: Option<worktree_dialog::WorktreeCreationSource>,
    endpoint: Result<LocalRecoveryEndpoint, String>, work: PreparationWork, deadline: Instant,
) -> Result<PreparedProjectRepository, ProjectPreparationFailure> {
    let ticket = work.ticket().clone();
    let result = (|| -> Result<PreparedProjectRepository, String> {
        if cancellation.is_cancelled() { return Err("Project loading was cancelled before admission.".into()); }
        if let Some(source) = retained.filter(|_| kind != Some(ProjectKind::Folder)) {
            let admission = source.edited_root_admission().map_err(|error| error.to_string())?;
            let (root, _) = source.recovery_roots().map_err(|error| error.to_string())?;
            admission.validate_for_root(&root, deadline).map_err(|error| error.to_string())?;
            if admission.ticket().phase() != AdmissionPhase::Active { return Err("The original source is retiring; no replacement was captured.".into()); }
            // No new lease/context on an ordinary refresh. Preserve actor ID,
            // whole admitted pair and original service/physical authority.
            work.finish_read_only();
            let loaded = load_project_with_source(path, Some(project_id), cancellation, kind, Some(source))?;
            return Ok(PreparedProjectRepository { loaded, prepared_creation: None });
        }
        let directory = path.canonicalize().map_err(|error| format!("Could not resolve project {}: {error}", path.display()))?;
        let folder = match kind {
            Some(ProjectKind::Folder) => true,
            Some(ProjectKind::Git) => false,
            None => crate::project_location::discover(&directory)?.kind == ProjectKind::Folder,
        };
        if folder {
            let loaded = load_project_with_source(&directory, Some(project_id), cancellation, Some(ProjectKind::Folder), None)?;
            work.finish_read_only();
            return Ok(PreparedProjectRepository { loaded, prepared_creation: None });
        }
        let service = RepositoryService::system("git");
        let candidate = service.discover(&directory, &cancellation).map_err(|error| error.to_string())?;
        if candidate.is_bare || !WorktreeAuthority::supported() {
            let loaded = read_loaded_repository(&service, candidate, None, Some(project_id), &cancellation)?;
            work.finish_read_only();
            return Ok(PreparedProjectRepository { loaded, prepared_creation: None });
        }
        let root = candidate.worktree_root.as_ref().ok_or("Selected repository has no worktree to retain.")?;
        let authority = WorktreeAuthority::capture(root).map_err(|error| error.to_string())?;
        let (original, _) = authority.recovery_roots().map_err(|error| error.to_string())?;
        let client = RecoveryPlacementClient::connect_local_until(endpoint?, deadline).map_err(|error| error.to_string())?;
        let owner = client.prepare_edited_root_owner(original).map_err(|error| error.to_string())?;
        let prepared = work.prepared(owner); // original window custody BEFORE any registration effect/message
        let scoped = service.with_worktree_authority(authority.clone()).map_err(|error| error.to_string())?;
        let repository = scoped.discover(root, &cancellation).map_err(|error| error.to_string())?;
        if repository.common_dir != candidate.common_dir { return Err("Repository identity changed during scoped discovery; no new source was published.".into()); }
        let loaded = read_loaded_repository(&scoped, repository, None, Some(project_id), &cancellation)?;
        authority.validate().map_err(|error| error.to_string())?;
        Ok(PreparedProjectRepository { loaded, prepared_creation: Some(PreparedProjectCreation { authority, admission: prepared }) })
    })();
    result.map_err(|error| ProjectPreparationFailure::with_ticket(ticket, error))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edited_root_admission::EditedRootAdmissions;

    #[test]
    fn folder_preparation_needs_no_recovery_endpoint_or_mutation_source() {
        let folder = tempfile::tempdir().unwrap();
        let project = ProjectId::new();
        let mut admissions = EditedRootAdmissions::default();
        let ticket = admissions.reserve().unwrap();
        let result = prepare_project_repository(folder.path(), project,
            CancellationToken::new(), Some(ProjectKind::Folder), None,
            Err("No local endpoint was captured".into()),
            ticket.begin_preparation().unwrap(), Instant::now());
        let prepared = result.unwrap();
        assert_eq!(prepared.loaded.project.id, project);
        assert_eq!(prepared.loaded.project.kind, ProjectKind::Folder);
        assert_eq!(prepared.loaded.project.repository, folder.path().canonicalize().unwrap());
        assert!(prepared.loaded.creation_source.is_none());
        assert!(prepared.prepared_creation.is_none());
        assert_eq!(ticket.phase(), AdmissionPhase::Settled);
        assert!(admissions.is_settled());
    }

    #[test]
    fn cancelled_preparation_retains_its_original_ticket_before_discovery() {
        let mut admissions = EditedRootAdmissions::default();
        let ticket = admissions.reserve().unwrap();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let directory = tempfile::tempdir().unwrap();
        let never_created = directory.path().join("missing");
        let failure = prepare_project_repository(&never_created, ProjectId::new(), cancel,
            Some(ProjectKind::Git), None, Err("No endpoint".into()),
            ticket.begin_preparation().unwrap(), Instant::now()).unwrap_err();
        assert!(failure.to_string().contains("cancelled before admission"));
        assert_eq!(failure.ticket().unwrap().id(), ticket.id());
        assert!(!ticket.has_worker());
        assert!(ticket.admitted().is_none());
        assert!(admissions.next_retirement().unwrap().is_none());
        assert!(admissions.is_settled());
        assert!(!never_created.exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn fresh_git_endpoint_failure_never_publishes_a_legacy_source() {
        if !crate::git_watch::tests::fixture_isolated(
            "app::project_source_admission::tests::fresh_git_endpoint_failure_never_publishes_a_legacy_source") { return; }
        let directory = tempfile::tempdir().unwrap();
        let command = crate::repository::CommandSpec::new("git").cwd(directory.path())
            .args(["init", "--template=", "-b", "main"]);
        crate::git_watch::tests::run_fixture_command(
            crate::git_watch::tests::isolated_fixture_command(command));
        let mut admissions = EditedRootAdmissions::default();
        let ticket = admissions.reserve().unwrap();
        let failure = prepare_project_repository(directory.path(), ProjectId::new(),
            CancellationToken::new(), Some(ProjectKind::Git), None,
            Err("captured local endpoint unavailable".into()),
            ticket.begin_preparation().unwrap(), Instant::now() + Duration::from_secs(5)).unwrap_err();
        assert_eq!(failure.to_string(), "captured local endpoint unavailable");
        assert_eq!(failure.ticket().unwrap().id(), ticket.id());
        assert!(ticket.admitted().is_none());
        assert!(!ticket.has_worker());
        assert!(admissions.next_retirement().unwrap().is_none());
        assert!(admissions.is_settled());
    }
}
