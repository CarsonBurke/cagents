//! Actual window inputs for the daemon-owned recovery placement reservation.
//! Capture clones on the UI thread; directory and Git checks belong to workers.
use super::*;
use crate::fs_safety::Root;
use std::{io, os::unix::fs::MetadataExt};

const MAX_RECOVERY_ROOTS: usize = 256;

/// This is an input census, never registration/effect authority. The blocking
/// producer must pass the resolved original roots into the authenticated daemon
/// reservation issuer. Its current catalog/lease also covers registered sibling
/// worktrees which this window has never opened.
#[derive(Clone, Debug)]
pub(super) struct WindowRecoveryRoots {
    projects: Vec<worktree_dialog::WorktreeCreationSource>,
    edited: Vec<Arc<Root>>,
}

pub(super) struct ResolvedWindowRoots {
    pub(super) edited_and_project_roots: Vec<Arc<Root>>,
    pub(super) common_directory_candidates: Vec<Arc<Root>>,
}

impl Cagents {
    /// Pure ingress capture: no metadata, directory opens or Git commands. The
    /// runtime getter returns its original admitted worker-opened file root,
    /// including a paused Locate source; descriptor.worktree is not authority.
    pub(super) fn recovery_window_roots(&self) -> Result<WindowRecoveryRoots, String> {
        if crate::device::is_remote() {
            return Err("Local filesystem recovery is unavailable for a remote device.".into());
        }
        let projects: Vec<_> = self.sidebar_model.projects.iter()
            .filter_map(|project| self.retained_project_creation_source(project.id))
            .take(MAX_RECOVERY_ROOTS + 1)
            .collect();
        let edited: Vec<_> = self.cards.values().filter_map(|runtime| match runtime {
            CardRuntime::Repository(repository) => repository.retained_filesystem_root(),
            _ => None,
        }).take(MAX_RECOVERY_ROOTS + 1).collect();
        if projects.len().checked_add(edited.len()).is_none_or(|count| count > MAX_RECOVERY_ROOTS) {
            return Err("Recovery exceeds the bounded current-window root census; no file was changed.".into());
        }
        Ok(WindowRecoveryRoots { projects, edited })
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
            roots.push(source);
            common.push(candidate);
        }
        let mut known = HashSet::new();
        let mut unique = Vec::new();
        for root in roots {
            check_cancelled()?;
            let metadata = root.metadata()?;
            if known.insert((metadata.dev(), metadata.ino())) { unique.push(root); }
        }
        if unique.len() > MAX_RECOVERY_ROOTS {
            return Err(io::Error::new(io::ErrorKind::Unsupported, "Recovery exceeds the bounded current-window root census; no file was changed."));
        }
        check_cancelled()?;
        Ok(ResolvedWindowRoots { edited_and_project_roots: unique, common_directory_candidates: common })
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
        let source = app.retained_project_creation_source(key.project_id).unwrap();
        let (original, common) = source.recovery_roots().unwrap();
        let resolved = app.recovery_window_roots().unwrap().resolve(&CancellationToken::new()).unwrap();
        assert!(resolved.edited_and_project_roots.iter().any(|root| Arc::ptr_eq(root, &original)));
        assert!(resolved.common_directory_candidates.iter().any(|root| Arc::ptr_eq(root, &common)));
        let delayed = app.recovery_window_roots().unwrap();
        app.retire_project_creation_source(key.project_id);
        assert!(delayed.resolve(&CancellationToken::new()).is_err(), "queued intake cannot replace a retired actor");
        // Already resolved roots are still inputs, not an effect capability.
        // A separately accepted daemon/protection context may keep these exact
        // objects for cleanup after the originating project is closed.
        assert!(resolved.edited_and_project_roots[0].metadata().unwrap().is_dir());
    }

    #[test]
    fn cancelled_census_does_not_resolve_any_project_or_open_a_replacement() {
        let token = CancellationToken::new(); token.cancel();
        let input = WindowRecoveryRoots { projects: Vec::new(), edited: Vec::new() };
        let error = match input.resolve(&token) { Ok(_) => panic!("cancelled intake succeeded"), Err(error) => error };
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
    }
}
