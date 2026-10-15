//! Actual window inputs for the daemon-owned recovery placement reservation.
//! Capture clones on the UI thread; directory and Git checks belong to workers.
use super::*;
use crate::fs_safety::{Root, RecoveryDirectoryStamp, recovery_directory_stamp};
use std::io;

const MAX_RECOVERY_ROOTS: usize = 256;

/// This is an input census, never registration/effect authority. The blocking
/// producer must pass the resolved original roots into the authenticated daemon
/// reservation issuer. Its current catalog/lease also covers registered sibling
/// worktrees which this window has never opened.
#[derive(Clone, Debug)]
pub(super) struct WindowRecoveryRoots {
    registry: uuid::Uuid,
    required_projects: Vec<(PathBuf, u32)>,
    projects: Vec<worktree_dialog::WorktreeCreationSource>,
    edited: Vec<Arc<Root>>,
}

pub(super) struct ResolvedWindowRoots {
    pub(super) registry: uuid::Uuid,
    /// The issuer must cover these observed logical instances with its CURRENT
    /// catalog, including Folder/bare/initial-load rows without a cached actor.
    /// These are initial admission expectations, not a permanently frozen epoch
    /// for an already accepted cleanup lease.
    pub(super) required_projects: Vec<(PathBuf, u32)>,
    pub(super) edited_and_project_roots: Vec<Arc<Root>>,
    common_directory_candidates: Vec<(RecoveryDirectoryStamp, Arc<Root>)>,
}

impl Cagents {
    /// Pure ingress capture: no metadata, directory opens or Git commands. The
    /// runtime getter returns its original admitted worker-opened file root,
    /// including a paused Locate source; descriptor.worktree is not authority.
    pub(super) fn recovery_window_roots(&self) -> Result<WindowRecoveryRoots, String> {
        if crate::device::is_remote() {
            return Err("Local filesystem recovery is unavailable for a remote device.".into());
        }
        if self.workspace_model.identity == crate::workspace_model::NO_REGISTRY {
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
            .filter_map(|project| self.retained_project_creation_source(project.id))
            .take(MAX_RECOVERY_ROOTS + 1)
            .collect();
        let edited: Vec<_> = self.cards.values().filter_map(|runtime| match runtime {
            CardRuntime::Repository(repository) => Some(repository.retained_filesystem_roots()),
            _ => None,
        }).flatten().take(MAX_RECOVERY_ROOTS + 1).collect();
        if projects.len().checked_add(edited.len()).is_none_or(|count| count > MAX_RECOVERY_ROOTS) {
            return Err("Recovery exceeds the bounded current-window root census; no file was changed.".into());
        }
        Ok(WindowRecoveryRoots { registry: self.workspace_model.identity, required_projects, projects, edited })
    }
}

impl ResolvedWindowRoots {
    /// A project's metadata is only a candidate for its own original source.
    /// The issuer/placement still must reject it if any checkout encloses it.
    pub(super) fn holding_candidates(&self, source: &Root) -> io::Result<Vec<Arc<Root>>> {
        let identity = recovery_directory_stamp(source)?;
        Ok(self.common_directory_candidates.iter().filter_map(|(owner, root)|
            (*owner == identity).then(|| root.clone())).collect())
    }
}

impl WindowRecoveryRoots {
    /// Blocking worker only. Every source is the original cached actor; no
    /// currently selected pane or pathname may supply a replacement. These
    /// temporary actor clones are dropped after reservation intake, so pending
    /// cleanup retains only exact roots plus its independent daemon lease and
    /// survives origin close or an unrelated project generation change.
    pub(super) fn resolve(self, cancellation: &CancellationToken) -> io::Result<ResolvedWindowRoots> {
        let check_cancelled = || if cancellation.is_cancelled() {
            Err(io::Error::new(io::ErrorKind::Interrupted, "Recovery intake was cancelled before any file effect."))
        } else { Ok(()) };
        check_cancelled()?;
        let mut roots = self.edited;
        let mut common = Vec::new();
        for project in self.projects {
            check_cancelled()?;
            let (source, candidate) = project.recovery_roots().map_err(|error| io::Error::other(error.to_string()))?;
            let identity = recovery_directory_stamp(&source)?;
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
        if unique.len() > MAX_RECOVERY_ROOTS {
            return Err(io::Error::new(io::ErrorKind::Unsupported, "Recovery exceeds the bounded current-window root census; no file was changed."));
        }
        check_cancelled()?;
        Ok(ResolvedWindowRoots { registry: self.registry, required_projects: self.required_projects, edited_and_project_roots: unique, common_directory_candidates: common })
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

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
        assert!(app.recovery_window_roots().is_err(), "a locally folded pending rank is not registry admission");
        app.workspace_model.projects[0].ordinal = 7;
        let source = app.retained_project_creation_source(key.project_id).unwrap();
        let (original, common) = source.recovery_roots().unwrap();
        let resolved = app.recovery_window_roots().unwrap().resolve(&CancellationToken::new()).unwrap();
        assert!(resolved.edited_and_project_roots.iter().any(|root| Arc::ptr_eq(root, &original)));
        assert!(resolved.holding_candidates(&original).unwrap().iter().any(|root| Arc::ptr_eq(root, &common)));
        let shared = app.workspace_model.projects.pop().unwrap();
        assert!(app.recovery_window_roots().is_err(), "unregistered provisional roots cannot silently disappear from the known domain");
        app.workspace_model.projects.push(shared);
        let delayed = app.recovery_window_roots().unwrap();
        app.retire_project_creation_source(key.project_id);
        assert!(delayed.resolve(&CancellationToken::new()).is_err(), "queued intake cannot replace a retired actor");
        // Initial-load/non-Git/bare rows have no creation Source. They still
        // carry a required registry identity, so an issuer must census the
        // owning daemon's current root catalog or explicitly refuse admission.
        let cold = app.recovery_window_roots().unwrap().resolve(&CancellationToken::new()).unwrap();
        assert_eq!(cold.registry, app.workspace_model.identity);
        assert_eq!(cold.required_projects, vec![(app.sidebar_model.project(key.project_id).unwrap().repository.clone(), 7)]);
        assert!(cold.common_directory_candidates.is_empty());

        // Already resolved roots are still inputs, not an effect capability.
        // A separately accepted daemon/protection context may keep these exact
        // objects for cleanup after the originating project is closed.
        assert!(resolved.edited_and_project_roots[0].metadata().unwrap().is_dir());
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
        let census = ResolvedWindowRoots { registry: uuid::Uuid::new_v4(), required_projects: Vec::new(),
            edited_and_project_roots: vec![a.clone(), b.clone()],
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
        let input = WindowRecoveryRoots { registry: uuid::Uuid::new_v4(), required_projects: Vec::new(), projects: Vec::new(), edited: Vec::new() };
        let error = match input.resolve(&token) { Ok(_) => panic!("cancelled intake succeeded"), Err(error) => error };
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
    }
}
