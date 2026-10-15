//! Actual window inputs for the daemon-owned recovery placement reservation.
//! Capture clones on the UI thread; directory and Git checks belong to workers.
use super::*;
use crate::fs_safety::{Root, RecoveryDirectoryStamp, recovery_directory_stamp};
use crate::recovery_domain::{HeldRecoveryRoot, IssuedRecoveryPlacement, LocalRecoveryEndpoint, ObservedProject, RecoveryIssueError, RecoveryPlacementClient, RecoveryPlacementInputs, RecoveryPlacementReceipt};
use crate::worktree_cleanup::Protection;
use std::{io, os::unix::ffi::OsStrExt, time::Instant};

const MAX_RECOVERY_ROOTS: usize = 256;

/// A failure before any Prepare has no custody. Once intake starts, preserve
/// its WHOLE original request/endpoint/root/protection receipt on uncertainty.
/// UI completion/queue owners must store that receipt before showing text,
/// even when the originating pane closes or its generation becomes stale.
#[derive(Debug)]
pub(super) enum WindowRecoveryIssueError {
    BeforeAdmission(io::Error),
    Intake(RecoveryIssueError),
}
impl std::fmt::Display for WindowRecoveryIssueError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self { Self::BeforeAdmission(error) => write!(f, "{error}"), Self::Intake(error) => write!(f, "{error}") }
    }
}
impl std::error::Error for WindowRecoveryIssueError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(match self { Self::BeforeAdmission(error) => error, Self::Intake(error) => error })
    }
}
impl From<io::Error> for WindowRecoveryIssueError {
    fn from(error: io::Error) -> Self { Self::BeforeAdmission(error) }
}
impl WindowRecoveryIssueError {
    pub(super) fn retained(&self) -> Option<&RecoveryPlacementReceipt> {
        match self { Self::BeforeAdmission(_) => None, Self::Intake(error) => error.retained() }
    }
    /// The error's text is separate from the mandatory retained owner. Do not
    /// discard the latter in a String conversion or stale-result early return.
    pub(super) fn into_parts(self) -> (io::Error, Option<RecoveryPlacementReceipt>) {
        match self { Self::BeforeAdmission(error) => (error, None), Self::Intake(error) => error.into_parts() }
    }
}

/// This is an input census, never registration/effect authority. The blocking
/// producer must pass the resolved original roots into the authenticated daemon
/// reservation issuer. Its current catalog/lease also covers registered sibling
/// worktrees which this window has never opened.
#[derive(Clone, Debug)]
pub(super) struct WindowRecoveryRoots {
    endpoint: LocalRecoveryEndpoint,
    registry: Option<uuid::Uuid>,
    required_projects: Vec<(PathBuf, u32)>,
    projects: Vec<WindowRecoveryProject>,
    edited: Vec<Arc<Root>>,
}

#[derive(Clone, Debug)]
struct WindowRecoveryProject {
    observed: (PathBuf, u32),
    source: worktree_dialog::WorktreeCreationSource,
}

/// Metadata proof is not a checkout exclusion. Its association must be checked
/// by the owning daemon against the original common-directory descriptor.
pub(super) struct ResolvedCommonMetadata {
    pub(super) observed: (PathBuf, u32),
    pub(super) directory: Arc<Root>,
}

pub(super) struct ResolvedWindowRoots {
    endpoint: LocalRecoveryEndpoint,
    pub(super) registry: Option<uuid::Uuid>,
    /// The issuer must cover these observed logical instances with its CURRENT
    /// catalog, including Folder/bare/initial-load rows without a cached actor.
    /// These are initial admission expectations, not a permanently frozen epoch
    /// for an already accepted cleanup lease.
    pub(super) required_projects: Vec<(PathBuf, u32)>,
    pub(super) edited_and_project_roots: Vec<Arc<Root>>,
    pub(super) common_metadata: Vec<ResolvedCommonMetadata>,
    common_directory_candidates: Vec<(RecoveryDirectoryStamp, Arc<Root>)>,
}

impl Cagents {
    /// Pure ingress capture: no metadata, directory opens or Git commands. The
    /// runtime getter returns its original admitted worker-opened file root,
    /// including a paused Locate source; descriptor.worktree is not authority.
    pub(super) fn recovery_window_roots(&self) -> Result<WindowRecoveryRoots, String> {
        let endpoint = LocalRecoveryEndpoint::capture(
            (self.workspace_model.identity != crate::workspace_model::NO_REGISTRY)
                .then_some(self.workspace_model.identity),
        ).map_err(|error| error.to_string())?;
        self.recovery_window_roots_for_endpoint(endpoint)
    }

    fn recovery_window_roots_for_endpoint(&self, endpoint: LocalRecoveryEndpoint) -> Result<WindowRecoveryRoots, String> {
        if crate::device::is_remote() {
            return Err("Local filesystem recovery is unavailable for a remote device.".into());
        }
        if self.workspace_model.identity == crate::workspace_model::NO_REGISTRY
            && (!self.sidebar_model.projects.is_empty() || !self.workspace_model.projects.is_empty()) {
            return Err("Wait for the shared project registry before changing files with recovery.".into());
        }
        let mut required_projects = Vec::new();
        for project in self.sidebar_model.projects.iter().take(MAX_RECOVERY_ROOTS + 1) {
            let Some(shared) = self.workspace_model.projects.iter().find(|shared| shared.repository == project.repository) else {
                return Err("Wait for the project to be registered before changing files with recovery.".into());
            };
            if shared.ordinal == crate::workspace_model::PENDING_ORDINAL {
                return Err("Wait for the registry to confirm the project before changing files with recovery.".into());
            }
            required_projects.push((shared.repository.clone(), shared.ordinal));
        }
        if required_projects.len() > MAX_RECOVERY_ROOTS {
            return Err("Recovery exceeds the bounded registered-project census; no file was changed.".into());
        }
        let projects: Vec<_> = self.sidebar_model.projects.iter()
            .filter_map(|project| {
                let source = self.retained_project_creation_source(project.id)?;
                let observed = required_projects.iter().find(|(key, _)| key == &project.repository)?.clone();
                Some(WindowRecoveryProject { observed, source })
            })
            .take(MAX_RECOVERY_ROOTS + 1)
            .collect();
        let edited: Vec<_> = self.cards.values().filter_map(|runtime| match runtime {
            CardRuntime::Repository(repository) => Some(repository.retained_filesystem_roots()),
            _ => None,
        }).flatten().take(MAX_RECOVERY_ROOTS + 1).collect();
        if projects.len().checked_add(edited.len()).is_none_or(|count| count > MAX_RECOVERY_ROOTS) {
            return Err("Recovery exceeds the bounded current-window root census; no file was changed.".into());
        }
        Ok(WindowRecoveryRoots { endpoint,
            // An empty initial window can bootstrap ONLY through the real
            // authenticated daemon catalog. None is unknown, never a new/nil
            // registry asserted as authority. Existing rows require identity.
            registry: (self.workspace_model.identity != crate::workspace_model::NO_REGISTRY)
                .then_some(self.workspace_model.identity),
            required_projects, projects, edited })
    }
}

impl ResolvedWindowRoots {
    /// Input roles only. Common metadata is separately associated with the
    /// original logical project; it must never become a checkout exclusion.
    fn issuer_inputs(&self) -> RecoveryPlacementInputs {
        let observed = |(repository, ordinal): &(PathBuf, u32)| ObservedProject {
            repository: repository.as_os_str().as_bytes().to_vec(), ordinal: *ordinal,
        };
        let mut held_roots: Vec<_> = self.edited_and_project_roots.iter()
            .cloned().map(HeldRecoveryRoot::checkout).collect();
        held_roots.extend(self.common_metadata.iter().map(|metadata|
            HeldRecoveryRoot::common_metadata(metadata.directory.clone(), observed(&metadata.observed))));
        RecoveryPlacementInputs {
            observed_registry: self.registry,
            observed_projects: self.required_projects.iter().map(observed).collect(),
            held_roots,
        }
    }

    /// A project's metadata is only a candidate for its own original source.
    /// The issuer/placement still must reject it if any checkout encloses it.
    pub(super) fn holding_candidates(&self, source: &Root) -> io::Result<Vec<Arc<Root>>> {
        let identity = recovery_directory_stamp(source)?;
        Ok(self.common_directory_candidates.iter().filter_map(|(owner, root)|
            (*owner == identity).then(|| root.clone())).collect())
    }
}

impl WindowRecoveryRoots {
    /// Actual blocking producer. Callers capture this window input together
    /// with the ORIGINAL effect source and its strong operation protection
    /// before queue admission. Neither a pathname nor the later active pane
    /// may replace those capabilities at the worker turn.
    ///
    /// The authenticated client owns the original endpoint/proof. Its candidate
    /// loop continues only after an explicitly settled effect-free refusal;
    /// unknown transfers stop with the daemon's negative obligation retained.
    /// The returned WHOLE issued placement retains that SAME issuer lease,
    /// issued parent and explicit retirement receipt. The operation owner must
    /// keep both context and receipt through cleanup; Drop grants no settlement.
    /// This method does not create H or lower any file effect.
    ///
    /// Source-only until the actual host route/controller and independent
    /// edited-root lifetime admission are composed. Observation-only census
    /// does not permit bypassing those required boundaries.
    pub(super) fn issue_context(
        self,
        source: Arc<Root>,
        protection: Arc<Option<Protection>>,
        payload_capacity: u8,
        cancellation: &CancellationToken,
        deadline: Instant,
    ) -> Result<IssuedRecoveryPlacement, WindowRecoveryIssueError> {
        if cancellation.is_cancelled() || Instant::now() >= deadline {
            return Err(io::Error::new(io::ErrorKind::Interrupted,
                "Recovery intake cancelled or expired before source validation.").into());
        }
        if !(1..=crate::recovery_domain::MAX_PAYLOADS).contains(&payload_capacity) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput,
                "Recovery requires a bounded positive payload reservation.").into());
        }
        let lease = protection.as_ref().as_ref().ok_or_else(|| io::Error::new(
            io::ErrorKind::Unsupported, "Recovery requires the original strong source protection before admission."))?;
        if !lease.protects_root(&source)? {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied,
                "Recovery source no longer matches its original operation protection.").into());
        }
        let resolved = self.resolve_until(cancellation, Some(deadline))?;
        let inputs = resolved.issuer_inputs();
        let commons = resolved.holding_candidates(&source)?;
        if cancellation.is_cancelled() || Instant::now() >= deadline {
            return Err(io::Error::new(io::ErrorKind::Interrupted,
                "Recovery intake cancelled or expired before connecting to its original endpoint.").into());
        }
        let client = RecoveryPlacementClient::connect_local(resolved.endpoint)?;
        client.reserve_with_candidates(inputs, source, protection, commons, payload_capacity,
            cancellation, deadline).map_err(WindowRecoveryIssueError::Intake)
    }

    /// Blocking worker only. Every source is the original cached actor; no
    /// currently selected pane or pathname may supply a replacement. These
    /// temporary actor clones are dropped after reservation intake, so pending
    /// cleanup retains only exact roots plus its independent daemon lease and
    /// survives origin close or an unrelated project generation change.
    pub(super) fn resolve(self, cancellation: &CancellationToken) -> io::Result<ResolvedWindowRoots> {
        self.resolve_until(cancellation, None)
    }

    fn resolve_until(self, cancellation: &CancellationToken, deadline: Option<Instant>) -> io::Result<ResolvedWindowRoots> {
        let check_cancelled = || if cancellation.is_cancelled() || deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            Err(io::Error::new(io::ErrorKind::Interrupted, "Recovery intake was cancelled or expired before any file effect."))
        } else { Ok(()) };
        check_cancelled()?;
        let mut roots = self.edited;
        let mut common = Vec::new();
        let mut common_metadata = Vec::new();
        for project in self.projects {
            check_cancelled()?;
            let (source, candidate) = project.source.recovery_roots().map_err(|error| io::Error::other(error.to_string()))?;
            let identity = recovery_directory_stamp(&source)?;
            common_metadata.push(ResolvedCommonMetadata { observed: project.observed, directory: candidate.clone() });
            common.push((identity, candidate));
            roots.push(source);
        }
        let mut known = HashSet::new();
        let mut unique = Vec::new();
        for root in roots {
            check_cancelled()?;
            let identity = recovery_directory_stamp(&root)?;
            if known.insert(identity) { unique.push(root); }
        }
        if unique.len().checked_add(common_metadata.len()).is_none_or(|count| count > MAX_RECOVERY_ROOTS) {
            return Err(io::Error::new(io::ErrorKind::Unsupported, "Recovery exceeds the bounded current-window root census; no file was changed."));
        }
        check_cancelled()?;
        Ok(ResolvedWindowRoots { endpoint: self.endpoint, registry: self.registry, required_projects: self.required_projects, edited_and_project_roots: unique, common_metadata, common_directory_candidates: common })
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    fn endpoint(registry: Option<uuid::Uuid>) -> LocalRecoveryEndpoint {
        LocalRecoveryEndpoint::fixture(PathBuf::from("/unconnected-private-fixture/daemon.sock"), registry).unwrap()
    }
    fn capture(app: &Cagents) -> Result<WindowRecoveryRoots, String> {
        app.recovery_window_roots_for_endpoint(endpoint((app.workspace_model.identity != crate::workspace_model::NO_REGISTRY).then_some(app.workspace_model.identity)))
    }

    #[tokio::test]
    async fn window_census_uses_published_source_and_refuses_retired_pending_intake() {
        const TEST: &str = "app::recovery_placement::tests::window_census_uses_published_source_and_refuses_retired_pending_intake";
        if !crate::git_watch::tests::fixture_isolated(TEST) { return; }
        let files = tempfile::tempdir().unwrap();
        let root = files.path().join("repository");
        super::super::git_binding::tests::init(&root, "original\n");
        let (mut app, key) = super::super::git_binding::tests::app_for(&root);
        let loaded = super::super::load_repository(&root, Some(key.project_id), CancellationToken::new()).unwrap();
        app.repository_generations.insert(key.project_id, 1);
        drop(app.update(Message::RepositoryLoaded {
            project_id: key.project_id, generation: 1, open_cards: false, result: Ok(Box::new(loaded)),
        }));
        let row = app.sidebar_model.project(key.project_id).unwrap();
        app.workspace_model.identity = uuid::Uuid::new_v4();
        app.workspace_model.projects.push(serde_json::from_value(serde_json::json!({
            "repository": row.repository, "name": row.name, "kind": row.kind, "ordinal": 7,
        })).unwrap());
        app.workspace_model.projects[0].ordinal = crate::workspace_model::PENDING_ORDINAL;
        assert!(capture(&app).is_err(), "a locally folded pending rank is not registry admission");
        app.workspace_model.projects[0].ordinal = 7;
        let original_registry = app.workspace_model.identity;
        app.workspace_model.identity = crate::workspace_model::NO_REGISTRY;
        assert!(capture(&app).is_err(), "existing rows cannot bootstrap under an unknown registry");
        app.workspace_model.identity = original_registry;
        let source = app.retained_project_creation_source(key.project_id).unwrap();
        let (original, common) = source.recovery_roots().unwrap();
        let resolved = capture(&app).unwrap().resolve(&CancellationToken::new()).unwrap();
        assert!(resolved.edited_and_project_roots.iter().any(|root| Arc::ptr_eq(root, &original)));
        assert!(resolved.holding_candidates(&original).unwrap().iter().any(|root| Arc::ptr_eq(root, &common)));
        assert_eq!(resolved.common_metadata.len(), 1);
        assert_eq!(resolved.common_metadata[0].observed, resolved.required_projects[0]);
        assert!(Arc::ptr_eq(&resolved.common_metadata[0].directory, &common));
        let shared = app.workspace_model.projects.pop().unwrap();
        assert!(capture(&app).is_err(), "unregistered provisional roots cannot silently disappear from the known domain");
        app.workspace_model.projects.push(shared);
        let delayed = capture(&app).unwrap();
        app.retire_project_creation_source(key.project_id);
        assert!(delayed.resolve(&CancellationToken::new()).is_err(), "queued intake cannot replace a retired actor");
        // Initial-load/non-Git/bare rows have no creation Source. They still
        // carry a required registry identity, so an issuer must census the
        // owning daemon's current root catalog or explicitly refuse admission.
        let cold = capture(&app).unwrap().resolve(&CancellationToken::new()).unwrap();
        assert_eq!(cold.registry, Some(app.workspace_model.identity));
        assert_eq!(cold.required_projects, vec![(app.sidebar_model.project(key.project_id).unwrap().repository.clone(), 7)]);
        assert!(cold.common_directory_candidates.is_empty());

        // Already resolved roots are still inputs, not an effect capability.
        // A separately accepted daemon/protection context may keep these exact
        // objects for cleanup after the originating project is closed.
        assert!(resolved.edited_and_project_roots[0].metadata().unwrap().is_dir());
        app.sidebar_model.projects.clear();
        app.workspace_model.projects.clear();
        app.workspace_model.identity = crate::workspace_model::NO_REGISTRY;
        let bootstrap = capture(&app).unwrap();
        assert_eq!(bootstrap.registry, None);
        assert!(bootstrap.required_projects.is_empty());
    }

    #[test]
    fn holding_metadata_candidates_cannot_be_borrowed_from_an_unrelated_project() {
        let files = tempfile::tempdir().unwrap();
        let root = |name: &str| {
            let path = files.path().join(name); std::fs::create_dir(&path).unwrap();
            Arc::new(Root::open(&path).unwrap())
        };
        let a = root("source-a"); let b = root("source-b"); let foreign = root("source-c");
        let ma = root("metadata-a"); let mb = root("metadata-b");
        let identity = |root: &Root| recovery_directory_stamp(root).unwrap();
        let registry = uuid::Uuid::new_v4();
        let census = ResolvedWindowRoots { endpoint: endpoint(Some(registry)), registry: Some(registry), required_projects: Vec::new(),
            edited_and_project_roots: vec![a.clone(), b.clone()], common_metadata: Vec::new(),
            common_directory_candidates: vec![(identity(&a), ma.clone()), (identity(&b), mb.clone())],
        };
        let candidates = census.holding_candidates(&a).unwrap();
        assert_eq!(candidates.len(), 1); assert!(Arc::ptr_eq(&candidates[0], &ma));
        let candidates = census.holding_candidates(&b).unwrap();
        assert_eq!(candidates.len(), 1); assert!(Arc::ptr_eq(&candidates[0], &mb));
        assert!(census.holding_candidates(&foreign).unwrap().is_empty());
    }

    #[test]
    fn cancelled_census_does_not_resolve_any_project_or_open_a_replacement() {
        let token = CancellationToken::new(); token.cancel();
        let registry = uuid::Uuid::new_v4();
        let input = WindowRecoveryRoots { endpoint: endpoint(Some(registry)), registry: Some(registry), required_projects: Vec::new(), projects: Vec::new(), edited: Vec::new() };
        let error = match input.resolve(&token) { Ok(_) => panic!("cancelled intake succeeded"), Err(error) => error };
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
    }
    #[test]
    fn issuer_inputs_preserve_same_root_roles_and_unknown_empty_bootstrap() {
        let files = tempfile::tempdir().unwrap();
        let source = Arc::new(Root::open(files.path()).unwrap());
        let repository = files.path().join("metadata");
        let registry = uuid::Uuid::new_v4();
        let census = ResolvedWindowRoots {
            endpoint: endpoint(Some(registry)), registry: Some(registry),
            required_projects: vec![(repository.clone(), 19)],
            edited_and_project_roots: vec![source.clone()],
            common_metadata: vec![ResolvedCommonMetadata {
                observed: (repository.clone(), 19), directory: source.clone(),
            }], common_directory_candidates: Vec::new(),
        };
        let inputs = census.issuer_inputs();
        assert_eq!(inputs.observed_registry, Some(registry));
        assert_eq!(inputs.observed_projects[0].repository, repository.as_os_str().as_bytes());
        assert_eq!(inputs.observed_projects[0].ordinal, 19);
        assert_eq!(inputs.held_roots.len(), 2);
        assert!(inputs.held_roots.iter().all(|held| Arc::ptr_eq(&held.root, &source)));
        assert!(matches!(inputs.held_roots[0].role, crate::recovery_domain::HeldRootRole::Checkout));
        assert!(matches!(&inputs.held_roots[1].role,
            crate::recovery_domain::HeldRootRole::CommonMetadata { project }
                if project == &inputs.observed_projects[0]));

        let empty = WindowRecoveryRoots { endpoint: endpoint(None), registry: None,
            required_projects: Vec::new(), projects: Vec::new(), edited: vec![source] }
            .resolve(&CancellationToken::new()).unwrap().issuer_inputs();
        assert_eq!(empty.observed_registry, None);
        assert!(empty.observed_projects.is_empty());
        assert_eq!(empty.held_roots.len(), 1);
    }

    #[test]
    fn actual_context_intake_requires_strong_original_protection_before_endpoint_io() {
        let files = tempfile::tempdir().unwrap();
        std::fs::write(files.path().join("unchanged"), b"original").unwrap();
        let source = Arc::new(Root::open(files.path()).unwrap());
        let registry = uuid::Uuid::new_v4();
        let input = || WindowRecoveryRoots {
            endpoint: endpoint(Some(registry)), registry: Some(registry),
            required_projects: Vec::new(), projects: Vec::new(), edited: vec![source.clone()],
        };
        let result = input().issue_context(source.clone(), Arc::new(None), 1,
            &CancellationToken::new(), Instant::now() + Duration::from_secs(5));
        assert!(matches!(result, Err(WindowRecoveryIssueError::BeforeAdmission(error)) if error.kind() == io::ErrorKind::Unsupported
            && error.to_string().contains("strong source protection")));
        let cancelled = CancellationToken::new(); cancelled.cancel();
        let result = input().issue_context(source.clone(), Arc::new(None), 1,
            &cancelled, Instant::now() + Duration::from_secs(5));
        assert!(matches!(result, Err(WindowRecoveryIssueError::BeforeAdmission(error)) if error.kind() == io::ErrorKind::Interrupted));
        assert_eq!(std::fs::read(files.path().join("unchanged")).unwrap(), b"original");
        assert_eq!(std::fs::read_dir(files.path()).unwrap().count(), 1);
    }

    #[test]
    fn actual_context_intake_contacts_only_the_captured_private_endpoint() {
        let files = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        std::fs::write(files.path().join("unchanged"), b"original").unwrap();
        let source = Arc::new(Root::open(files.path()).unwrap());
        let registry = uuid::Uuid::new_v4();
        let protection = Arc::new(Some(crate::worktree_cleanup::fixture_protection(
            runtime.path(), source.path(), false).unwrap()));
        let input = WindowRecoveryRoots {
            endpoint: LocalRecoveryEndpoint::fixture(runtime.path().join("absent-daemon.sock"), Some(registry)).unwrap(),
            registry: Some(registry), required_projects: Vec::new(), projects: Vec::new(), edited: vec![source.clone()],
        };
        let result = input.issue_context(source, protection, 1, &CancellationToken::new(),
            Instant::now() + Duration::from_secs(5));
        assert!(matches!(result, Err(WindowRecoveryIssueError::BeforeAdmission(error)) if error.kind() == io::ErrorKind::NotFound));
        assert_eq!(std::fs::read(files.path().join("unchanged")).unwrap(), b"original");
        assert_eq!(std::fs::read_dir(files.path()).unwrap().count(), 1);
        assert!(!runtime.path().join("absent-daemon.sock").exists());
    }

}
