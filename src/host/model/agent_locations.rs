//! Mutable logical placement and a verified current spelling of an agent's
//! approved directory. Accepted specs and process descriptors remain immutable.
use std::{fs::File, os::unix::fs::OpenOptionsExt, path::{Path, PathBuf}};
use rusqlite::{Connection, OptionalExtension, params};
use crate::{agent_execution::{Agent, DirectoryIdentity}, model::AgentId, project_recovery::Relocation};
use super::{Registry, execution::{decode, encode}, repository_key};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Location {
    pub repository: PathBuf,
    pub directory: PathBuf,
    pub blocked_directory: Option<PathBuf>,
}

pub(super) fn migrate(db: &Connection) -> Result<(), String> {
    let tx = rusqlite::Transaction::new_unchecked(db, rusqlite::TransactionBehavior::Immediate).map_err(|error| error.to_string())?;
    tx.execute_batch("CREATE TABLE IF NOT EXISTS agent_locations (
        agent TEXT PRIMARY KEY REFERENCES agents(id) ON DELETE CASCADE,
        repository TEXT NOT NULL,
        directory TEXT NOT NULL,
        blocked_directory TEXT
    ) STRICT;").map_err(|error| error.to_string())?;
    let records = tx.prepare("SELECT record FROM agents").map_err(|error| error.to_string())?.query_map([], |row| row.get::<_,String>(0)).and_then(Iterator::collect::<rusqlite::Result<Vec<_>>>).map_err(|error| error.to_string())?;
    for record in records { insert(&tx, &decode::<Agent>(&record)?)?; }
    tx.pragma_update(None, "user_version", 13).map_err(|error| error.to_string())?;
    tx.commit().map_err(|error| error.to_string())
}

fn insert(db: &Connection, agent: &Agent) -> Result<(), String> {
    db.execute("INSERT OR IGNORE INTO agent_locations(agent,repository,directory) VALUES (?1,?2,?3)", params![agent.id.to_string(), repository_key(&agent.spec.target.repository)?, repository_key(&agent.spec.target.directory)?]).map_err(|error| error.to_string())?;
    Ok(())
}

/// Pin a real selected directory at first acceptance. Legacy records keep
/// their existing policy; opening a replacement old path cannot reapprove it.
pub(super) fn insert_new(db: &Connection, agent: &Agent) -> Result<(), String> {
    insert(db, agent)?;
    if let Ok(directory) = open_directory(&agent.spec.target.directory) {
        let identity = DirectoryIdentity::from_metadata(&directory.metadata().map_err(|error| error.to_string())?);
        db.execute("INSERT INTO agent_policies(agent,directory) VALUES (?1,?2) ON CONFLICT(agent) DO UPDATE SET directory=excluded.directory WHERE agent_policies.directory IS NULL", params![agent.id.to_string(), encode(&identity)?]).map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn open_directory(path: &Path) -> std::io::Result<File> {
    crate::secure_file::reject_symlink_components(path)?;
    std::fs::OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC).open(path)
}

pub(super) fn read(db: &Connection, agent: &Agent) -> Result<Location, String> {
    db.query_row("SELECT repository,directory,blocked_directory FROM agent_locations WHERE agent=?1", [agent.id.to_string()], |row| Ok(Location { repository: PathBuf::from(row.get::<_,String>(0)?), directory: PathBuf::from(row.get::<_,String>(1)?), blocked_directory: row.get::<_,Option<String>>(2)?.map(PathBuf::from) })).optional().map_err(|error| error.to_string()).map(|location| location.unwrap_or_else(|| Location { repository: agent.spec.target.repository.clone(), directory: agent.spec.target.directory.clone(), blocked_directory: None }))
}

pub(super) fn relocate(db: &Connection, agent: &Agent, relocation: &Relocation) -> Result<bool, String> {
    let previous = read(db, agent)?;
    if previous.repository != relocation.repository { return Ok(false); }
    insert(db, agent)?;
    let candidate = relocation.mapped(previous.blocked_directory.as_deref().unwrap_or(&previous.directory));
    let approved: Option<String> = db.query_row("SELECT directory FROM agent_policies WHERE agent=?1", [agent.id.to_string()], |row| row.get(0)).optional().map_err(|error| error.to_string())?.flatten();
    let approved = approved.map(|value| decode::<DirectoryIdentity>(&value)).transpose()?;
    let verified = approved.is_some_and(|approved| open_directory(&candidate).and_then(|directory| directory.metadata()).is_ok_and(|metadata| DirectoryIdentity::from_metadata(&metadata) == approved));
    let (directory, blocked) = if verified { (candidate, None) } else { (previous.directory, Some(candidate)) };
    db.execute("UPDATE agent_locations SET repository=?2,directory=?3,blocked_directory=?4 WHERE agent=?1", params![agent.id.to_string(), repository_key(&relocation.destination)?, repository_key(&directory)?, blocked.as_deref().map(repository_key).transpose()?]).map_err(|error| error.to_string())?;
    Ok(true)
}

impl Registry {
    pub(crate) fn retained_agent_execution_directory(&self, retained: &[crate::agent_execution::HostedSession]) -> Result<Option<PathBuf>, String> {
        // The existing source-lineage check is required before resolving any
        // operational path for an adopted holder.
        if self.retained_agent_directory(retained)?.is_none() { return Ok(None); }
        let Some(first) = retained.first() else { return Ok(None); };
        let record: Option<String> = self.db.query_row("SELECT record FROM agents WHERE session_id=?1 OR id=(SELECT agent FROM agent_launches WHERE session_id=?1)", [first.id().to_string()], |row| row.get(0)).optional().map_err(|error| error.to_string())?;
        record.map(|record| self.agent_execution_directory(decode::<Agent>(&record)?.id)).transpose()
    }

    pub(crate) fn agent_location(&self, id: AgentId) -> Result<Location, String> { read(&self.db, &self.agent(id)?) }

    /// A path is usable only while it still names the exact approved inode.
    /// This check is repeated by ExecutionContext after opening the directory.
    pub(crate) fn agent_execution_directory(&self, id: AgentId) -> Result<PathBuf, String> {
        let location = self.agent_location(id)?;
        if let Some(candidate) = location.blocked_directory {
            return Err(format!("Agent {id}'s located directory {} does not match its approved directory identity. Close this agent and start a new one for the replacement folder.", candidate.display()));
        }
        if let Some(approved) = self.agent_directory(id)? {
            let directory = open_directory(&location.directory).map_err(|error| format!("Agent {id}'s approved directory {} is unavailable: {error}", location.directory.display()))?;
            let actual = DirectoryIdentity::from_metadata(&directory.metadata().map_err(|error| error.to_string())?);
            if actual != approved { return Err(format!("Agent {id}'s directory {} was replaced; existing process authority cannot be transferred to it", location.directory.display())); }
        }
        Ok(location.directory)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{agent::Provider, agent_execution::{Accepted, Destination, Origin, Spec, Submit, Surface, Target}, model::ProjectKind, workspace_model::{ModelEdit, views::Scope}};

    fn accepted(registry: &mut Registry, repository: &Path, key: &str) -> Accepted {
        registry.apply(&ModelEdit::OpenProject { repository: repository.into(), name: key.into(), kind: ProjectKind::Folder }).unwrap();
        let placement = Scope::Worktree { repository: repository.into(), path: repository.into() };
        registry.apply(&ModelEdit::SeedView { scope: placement.clone(), panes: Vec::new(), closed: Vec::new() }).unwrap();
        registry.submit_agent(&Origin::User, &Submit { name: None, destination: Destination::New(Spec { provider: Provider::Claude, account: None, target: Target { repository: repository.into(), directory: repository.into(), placement: Some(placement) }, surface: Surface::Chat, model: None, permission_mode: None, reasoning_effort: None }), prompt: "fixture only, no provider starts".into(), idempotency_key: key.into() }, 1).unwrap()
    }

    #[test]
    fn same_inode_locate_preserves_accepted_spec_and_followup_dispatch_after_restart() {
        let fixture = tempfile::tempdir().unwrap();
        let old = fixture.path().join("old"); std::fs::create_dir(&old).unwrap();
        let path = fixture.path().join("registry.sqlite3");
        let mut registry = Registry::at(&path).unwrap();
        let accepted = accepted(&mut registry, &old, "original");
        let proof = registry.agent_directory(accepted.agent.id).unwrap().unwrap();
        let moved = fixture.path().join("moved"); std::fs::rename(&old, &moved).unwrap();
        let relocation = Relocation { repository: old.clone(), destination: moved.clone(), old_root: old.clone(), new_root: moved.clone() };
        registry.apply(&ModelEdit::RelocateProject { relocation }).unwrap();
        assert_eq!(registry.agent(accepted.agent.id).unwrap().spec, accepted.agent.spec);
        assert_eq!(registry.agent_location(accepted.agent.id).unwrap().repository, moved);
        assert_eq!(registry.agent_execution_directory(accepted.agent.id).unwrap(), moved);
        registry.validate_agent_dispatch(&accepted).unwrap();
        assert!(registry.claim_agent_dispatch(&accepted, 2).unwrap());
        let started = registry.update_agent_run(accepted.run.id, accepted.run.revision, crate::agent_execution::Update::Started { provider_key: "fixture".into() }, 3).unwrap();
        registry.update_agent_run(started.id, started.revision, crate::agent_execution::Update::Finished { state: crate::agent_execution::State::Succeeded, result: Default::default() }, 4).unwrap();
        let followup = registry.submit_agent(&Origin::User, &Submit { name: None, destination: Destination::Existing(accepted.agent.id), prompt: "followup fixture".into(), idempotency_key: "followup".into() }, 5).unwrap();
        assert_eq!(followup.agent.spec, accepted.agent.spec);
        registry.validate_agent_dispatch(&followup).unwrap();
        assert!(registry.claim_agent_dispatch(&followup, 6).unwrap());
        drop(registry);
        let registry = Registry::at(&path).unwrap();
        assert_eq!(registry.agent_directory(accepted.agent.id).unwrap(), Some(proof));
        assert_eq!(registry.agent_execution_directory(accepted.agent.id).unwrap(), moved);
        assert_eq!(registry.agent(accepted.agent.id).unwrap().spec, accepted.agent.spec);
    }

    #[test]
    fn reused_old_path_is_a_distinct_project_and_cannot_redirect_original_agent() {
        let fixture = tempfile::tempdir().unwrap();
        let old = fixture.path().join("old"); std::fs::create_dir(&old).unwrap();
        let mut registry = Registry::at(&fixture.path().join("registry.sqlite3")).unwrap();
        let original = accepted(&mut registry, &old, "original");
        let moved = fixture.path().join("moved"); std::fs::rename(&old, &moved).unwrap();
        registry.apply(&ModelEdit::RelocateProject { relocation: Relocation { repository: old.clone(), destination: moved.clone(), old_root: old.clone(), new_root: moved.clone() } }).unwrap();
        std::fs::create_dir(&old).unwrap();
        let replacement = accepted(&mut registry, &old, "replacement");
        assert_eq!(registry.agent_execution_directory(original.agent.id).unwrap(), moved);
        assert_eq!(registry.agent_execution_directory(replacement.agent.id).unwrap(), old);
        assert_ne!(registry.agent_directory(original.agent.id).unwrap(), registry.agent_directory(replacement.agent.id).unwrap());
        assert!(super::super::execution::require_placement(&registry.db, &replacement.agent.placement, &moved).is_err());
        registry.validate_agent_dispatch(&original).unwrap();
        registry.validate_agent_dispatch(&replacement).unwrap();
    }

    #[test]
    fn wrong_inode_locate_retains_state_and_refuses_existing_execution() {
        let fixture = tempfile::tempdir().unwrap();
        let old = fixture.path().join("old"); std::fs::create_dir(&old).unwrap();
        let mut registry = Registry::at(&fixture.path().join("registry.sqlite3")).unwrap();
        let accepted = accepted(&mut registry, &old, "original");
        let hidden = fixture.path().join("original-inode"); std::fs::rename(&old, &hidden).unwrap();
        let replacement = fixture.path().join("replacement"); std::fs::create_dir(&replacement).unwrap();
        registry.apply(&ModelEdit::RelocateProject { relocation: Relocation { repository: old.clone(), destination: replacement.clone(), old_root: old.clone(), new_root: replacement.clone() } }).unwrap();
        let location = registry.agent_location(accepted.agent.id).unwrap();
        assert_eq!(location.repository, replacement);
        assert_eq!(location.directory, old);
        assert_eq!(location.blocked_directory, Some(replacement));
        assert!(registry.agent_execution_directory(accepted.agent.id).unwrap_err().contains("approved directory identity"));
        assert_eq!(registry.agent(accepted.agent.id).unwrap().spec, accepted.agent.spec);
        // Recreating the original spelling is never a global alias grant.
        std::fs::create_dir(&old).unwrap();
        assert!(registry.agent_execution_directory(accepted.agent.id).is_err());
    }
}
