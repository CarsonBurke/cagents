//! Ownership rules for closing project sessions on the daemon's machine.
use crate::model::{ProjectKind, TerminalId};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProjectSessionClosure {
    pub scope: ProjectSessionScope,
    pub ownership: ProjectSessionOwnership,
}

#[cfg(any(test, feature = "test-support"))]
impl ProjectSessionClosure {
    pub fn for_roots(roots: Vec<PathBuf>) -> Self {
        Self {
            scope: ProjectSessionScope::new(roots, Vec::new()),
            ownership: ProjectSessionOwnership::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProjectSessionScope {
    roots: Vec<PathBuf>,
    protected: Vec<PathBuf>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ProjectSessionOwnership {
    pub owned: BTreeSet<TerminalId>,
    pub protected: BTreeSet<TerminalId>,
    pub directories: BTreeMap<TerminalId, Vec<PathBuf>>,
    #[serde(skip)]
    pub local_database: Option<PathBuf>,
}

#[cfg(any(feature = "ui", feature = "host"))]
impl ProjectSessionOwnership {
    pub fn extend_saved(
        &mut self,
        owners: Vec<(TerminalId, crate::persistence::SavedSessionOwner)>,
        project: &Path,
    ) {
        for (id, owner) in owners {
            match owner {
                crate::persistence::SavedSessionOwner::Project(path) if path == project => {
                    self.owned.insert(id);
                }
                crate::persistence::SavedSessionOwner::Project(_) => {
                    self.protected.insert(id);
                }
                crate::persistence::SavedSessionOwner::Directory(path) => {
                    self.directories.entry(id).or_default().push(path);
                }
            }
        }
    }

    pub fn contains(&self, id: TerminalId, directory: &Path, scope: &ProjectSessionScope) -> bool {
        if self.protected.contains(&id) {
            return false;
        }
        if let Some(directories) = self.directories.get(&id) {
            // A retained view wins when windows disagree about ownership.
            if directories.iter().any(|path| !scope.contains(path)) {
                return false;
            }
            return true;
        }
        self.owned.contains(&id) || scope.contains(directory)
    }
}

#[cfg(any(test, feature = "test-support"))]
thread_local! {
    static SCOPE_RESOLUTIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// How many paths this thread has resolved on disk for a project scope.
#[cfg(any(test, feature = "test-support"))]
pub fn scope_resolutions() -> usize {
    SCOPE_RESOLUTIONS.get()
}

/// Resolves `path` on disk, as every root and directory a scope compares is.
/// A device window's paths are the device's, as its daemon reported them,
/// and resolving them here would read this computer's files instead.
fn resolve_scope_path(path: &Path) -> Option<PathBuf> {
    if crate::device::is_remote() {
        return None;
    }
    #[cfg(any(test, feature = "test-support"))]
    SCOPE_RESOLUTIONS.set(SCOPE_RESOLUTIONS.get() + 1);
    path.canonicalize().ok()
}

impl ProjectSessionScope {
    pub fn new(roots: Vec<PathBuf>, protected: Vec<PathBuf>) -> Self {
        let canonical = |paths: Vec<PathBuf>| {
            paths
                .into_iter()
                .map(|path| resolve_scope_path(&path).unwrap_or(path))
                .collect()
        };
        Self {
            roots: canonical(roots),
            protected: canonical(protected),
        }
    }

    pub fn contains(&self, directory: &Path) -> bool {
        let directory = resolve_scope_path(directory).unwrap_or_else(|| directory.to_path_buf());
        let depth = |roots: &[PathBuf]| {
            roots
                .iter()
                .filter(|root| directory.starts_with(root))
                .map(|root| root.components().count())
                .max()
        };
        match (depth(&self.roots), depth(&self.protected)) {
            (Some(owned), Some(protected)) => owned > protected,
            (Some(_), None) => true,
            _ => false,
        }
    }
}

pub fn shared_project_session_scope(
    roots: &[PathBuf],
    repository: &Path,
    projects: &[crate::workspace_model::ModelProject],
    list_worktree_paths: impl Fn(&Path) -> Result<Vec<PathBuf>, String>,
) -> Result<ProjectSessionScope, String> {
    let mut protected = Vec::new();
    for project in projects {
        if project.repository == repository {
            continue;
        }
        match project.kind {
            ProjectKind::Folder => protected.push(project.repository.clone()),
            ProjectKind::Git => {
                let worktrees = list_worktree_paths(&project.repository)
                    .map_err(|error| format!("Could not verify worktrees owned by {} before removing this project: {error}", project.name))?;
                protected.extend(worktrees);
            }
        }
    }
    Ok(ProjectSessionScope::new(roots.to_vec(), protected))
}

#[cfg(any(feature = "ui", feature = "host"))]
pub fn add_saved_owners(
    ownership: &mut ProjectSessionOwnership,
    repository: &Path,
    paths: std::io::Result<Vec<PathBuf>>,
) -> Result<(), String> {
    let paths = paths.map_err(|error| {
        format!("Could not locate other windows before closing sessions: {error}")
    })?;
    for path in paths {
        if ownership.local_database.as_ref() == Some(&path) {
            continue;
        }
        match std::fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(format!(
                    "Could not inspect ownership in {}: {error}",
                    path.display()
                ));
            }
            Ok(_) => {}
        }
        let owners = crate::persistence::read_project_session_owners(&path).map_err(|error| {
            format!("Could not verify ownership in {}: {error}", path.display())
        })?;
        ownership.extend_saved(owners, repository);
    }
    Ok(())
}
