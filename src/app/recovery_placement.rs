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
#[derive(Clone)]
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
    pub(super) fn resolve(self) -> io::Result<ResolvedWindowRoots> {
        let mut roots = self.edited;
        let mut common = Vec::new();
        for project in self.projects {
            let (source, candidate) = project.recovery_roots().map_err(|error| io::Error::other(error.to_string()))?;
            roots.push(source);
            common.push(candidate);
        }
        let mut known = HashSet::new();
        let mut unique = Vec::new();
        for root in roots {
            let metadata = root.metadata()?;
            if known.insert((metadata.dev(), metadata.ino())) { unique.push(root); }
        }
        if unique.len() > MAX_RECOVERY_ROOTS {
            return Err(io::Error::new(io::ErrorKind::Unsupported, "Recovery exceeds the bounded current-window root census; no file was changed."));
        }
        Ok(ResolvedWindowRoots { edited_and_project_roots: unique, common_directory_candidates: common })
    }
}
