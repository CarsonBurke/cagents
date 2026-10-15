//! Exact path migration for a project the user explicitly located again.
//! Runtime/session identities and relative editor paths are preserved.
use std::path::{Path, PathBuf};
use serde::{Deserialize, Serialize};
use crate::{model::{CardKind, TerminalSession, TerminalContinuity}, workspace_model::views::{Scope, View}};

/// A user-visible project selected from one exact registry incarnation.
/// Paths and ranks can both be reused; neither is sufficient alone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectInstance {
    pub registry: uuid::Uuid,
    pub repository: PathBuf,
    pub ordinal: u32,
}
impl ProjectInstance {
    pub fn validate(&self) -> Result<(), String> {
        if self.registry == crate::workspace_model::NO_REGISTRY || self.ordinal == crate::workspace_model::PENDING_ORDINAL || !self.repository.is_absolute()
            || self.repository.components().any(|component| matches!(component, std::path::Component::ParentDir)) {
            return Err("Project removal requires its original registry and assigned project identity".into());
        }
        Ok(())
    }
}

/// Backend process incarnation is distinct from the durable logical actor.
/// Keep this shared DTO independent from host-only holder implementation types.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderIncarnation { pub pid: i32, pub start: u64 }
impl ProviderIncarnation {
    #[cfg(feature = "host")]
    pub(crate) fn from_process(process: crate::daemon::holder_registry::Process) -> Self { Self { pid: process.pid, start: process.start } }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionClose {
    pub source: crate::agent_execution::HostedSession,
    pub execution_instance: Option<uuid::Uuid>,
    pub provider: Option<ProviderIncarnation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Relocation {
    pub repository: PathBuf,
    pub destination: PathBuf,
    pub old_root: PathBuf,
    pub new_root: PathBuf,
}

impl Relocation {
    pub fn validate(&self) -> Result<(), String> {
        for path in [&self.repository, &self.destination, &self.old_root, &self.new_root] {
            if !path.is_absolute() || path.components().any(|component| matches!(component, std::path::Component::ParentDir)) {
                return Err("project locations must be absolute normalized paths".into());
            }
        }
        // Git metadata can live outside its checkout. Logical ownership comes
        // from the exact persisted project; operational authority is verified
        // separately against pinned directory/process identities.
        Ok(())
    }

    pub fn verification_only(&self) -> bool { self.repository == self.destination && self.old_root == self.new_root }

    pub fn path(&self, path: &mut PathBuf) {
        if *path == self.repository { *path = self.destination.clone(); }
        else if let Ok(relative) = path.strip_prefix(&self.old_root) { *path = self.new_root.join(relative); }
    }
    pub fn mapped(&self, path: &Path) -> PathBuf {
        let mut path = path.to_path_buf(); self.path(&mut path); path
    }
    pub fn scope(&self, scope: &mut Scope) {
        if let Scope::Worktree { repository, path } = scope
            && *repository == self.repository {
            *repository = self.destination.clone(); self.path(path);
        }
    }
    pub fn terminal(&self, session: &mut TerminalSession) {
        if let TerminalContinuity::RelaunchAvailable { recipe, .. } = &mut session.continuity {
            self.path(&mut recipe.working_directory);
        }
    }
    pub fn card(&self, kind: &mut CardKind) {
        match kind {
            CardKind::Repository(view) => { if view.repository == self.repository { view.repository = self.destination.clone(); self.path(&mut view.worktree); } }
            CardKind::Terminal(session) => self.terminal(session),
            // Launch recipes are immutable process provenance. Logical pane
            // scopes move separately, without claiming a running process was
            // launched in the selected replacement directory.
            CardKind::Chat(_) => {},
            CardKind::PullRequests(view) => { if view.repository == self.repository { view.repository = self.destination.clone(); } },
            _ => {}
        }
    }
    pub fn view(&self, view: &mut View) {
        self.scope(&mut view.scope);
        for pane in &mut view.panes {
            self.card(&mut pane.kind);
            if let Some(path) = &mut pane.scope { self.path(path); }
        }
    }
    pub fn agent(&self, agent: &mut crate::agent_execution::Agent) {
        // The accepted spec and provider descriptors are immutable. Moving a
        // logical placement must not change or broaden their launch authority.
        self.scope(&mut agent.placement);

    }
}

/// A missing root is a specific filesystem fact, not any arbitrary Git error.
/// Permissions and temporary read failures must not be mislabeled as deletion.
pub fn missing_reason(root: &Path) -> Option<String> {
    match std::fs::metadata(root) {
        Ok(metadata) if !metadata.is_dir() => Some("The project path is no longer a directory".into()),
        Err(error) if matches!(error.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory) => Some(error.to_string()),
        _ => None,
    }
}

pub fn root_missing(root: &Path) -> bool { missing_reason(root).is_some() }

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn missing_directory_recovers_when_it_reappears() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("project");
        std::fs::create_dir(&root).unwrap(); assert!(!root_missing(&root));
        std::fs::remove_dir(&root).unwrap(); assert!(root_missing(&root));
        std::fs::create_dir(&root).unwrap(); assert!(!root_missing(&root));
    }
    #[test]
    fn replaced_directory_is_missing_and_details_preserve_the_real_error() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().join("project");
        std::fs::write(&root, "replacement").unwrap();
        assert!(root_missing(&root));
        assert_eq!(missing_reason(&root).as_deref(), Some("The project path is no longer a directory"));
        let absent = fixture.path().join("absent");
        let actual = std::fs::metadata(&absent).unwrap_err().to_string();
        assert_eq!(missing_reason(&absent), Some(actual));
    }

    #[test]
    fn locating_repoints_cards_but_keeps_relative_file_and_unrelated_paths() {
        let relocation = Relocation { repository: "/old/repo/.git".into(), destination: "/new/repo/.git".into(), old_root: "/old/repo".into(), new_root: "/new/repo".into() };
        relocation.validate().unwrap();
        let mut card = CardKind::Repository(crate::model::RepositoryView { project: Default::default(), repository: relocation.repository.clone(), worktree: "/old/repo/worktrees/one".into(), selected_file: Some("src/main.rs".into()) });
        relocation.card(&mut card);
        let CardKind::Repository(card) = card else { panic!() };
        assert_eq!(card.repository, relocation.destination);
        assert_eq!(card.worktree, Path::new("/new/repo/worktrees/one"));
        assert_eq!(card.selected_file.as_deref(), Some(Path::new("src/main.rs")));
        assert_eq!(relocation.mapped(Path::new("/old/repository-other")), Path::new("/old/repository-other"));
    }
}
