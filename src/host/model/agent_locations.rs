//! Mutable logical placement and a verified current spelling of an agent's
//! approved directory. Accepted specs and process descriptors remain immutable.
use super::{
    Registry,
    execution::{decode, encode},
    repository_key,
};
use crate::{
    agent_execution::{Agent, DirectoryIdentity},
    model::AgentId,
    project_recovery::Relocation,
};
use rusqlite::{Connection, OptionalExtension, params};
use std::{
    fs::File,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Location {
    pub repository: PathBuf,
    pub directory: PathBuf,
    pub blocked_directory: Option<PathBuf>,
}

/// Snapshot under the registry lock, then verify outside HostState and the
/// writer transaction. The exact descriptor stays pinned through commit.
#[derive(Debug)]
pub(crate) struct RelocationInput {
    agent: AgentId,
    previous: Location,
    approved: Option<DirectoryIdentity>,
    candidate: PathBuf,
}
#[derive(Debug)]
pub(crate) struct AgentMove {
    input: RelocationInput,
    identity: Option<DirectoryIdentity>,
    _directory: Option<std::sync::Arc<File>>,
}
impl RelocationInput {
    pub(crate) fn verify(self) -> AgentMove {
        let directory = open_directory(&self.candidate)
            .ok()
            .map(std::sync::Arc::new);
        let identity = directory
            .as_ref()
            .and_then(|directory| directory.metadata().ok())
            .map(|metadata| DirectoryIdentity::from_metadata(&metadata));
        AgentMove {
            input: self,
            identity,
            _directory: directory,
        }
    }
}

pub(super) fn backfill(tx: &rusqlite::Transaction<'_>) -> Result<(), String> {
    let records = tx
        .prepare("SELECT record FROM agents")
        .map_err(|error| error.to_string())?
        .query_map([], |row| row.get::<_, String>(0))
        .and_then(Iterator::collect::<rusqlite::Result<Vec<_>>>)
        .map_err(|error| error.to_string())?;
    for record in records {
        insert(tx, &decode::<Agent>(&record)?)?;
    }
    Ok(())
}

fn insert(db: &Connection, agent: &Agent) -> Result<(), String> {
    db.execute(
        "INSERT OR IGNORE INTO agent_locations(agent,repository,directory) VALUES (?1,?2,?3)",
        params![
            agent.id.to_string(),
            repository_key(&agent.spec.target.repository)?,
            repository_key(&agent.spec.target.directory)?
        ],
    )
    .map_err(|error| error.to_string())?;
    Ok(())
}

/// Nonserialized observation of the caller's actual retained descriptor.
/// Construct on the owning worker BEFORE taking Registry/the writer TX. The
/// private fields prevent a recipe/path or bare inode tuple granting approval.
#[derive(Debug)]
pub(crate) struct AcceptedDirectory {
    path:PathBuf,
    identity:DirectoryIdentity,
    _directory:std::sync::Arc<File>,
}
impl AcceptedDirectory {
    pub(crate) fn observe(path:&Path,directory:std::sync::Arc<File>)->Result<Self,String> {
        Self::observe_with(path,directory,|path|std::fs::symlink_metadata(path))
    }
    fn observe_with(path:&Path,directory:std::sync::Arc<File>,read_named:impl FnOnce(&Path)->std::io::Result<std::fs::Metadata>)->Result<Self,String> {
        if !path.is_absolute()||path.components().any(|component|matches!(component,std::path::Component::ParentDir)) {
            return Err("The selected agent directory must have its original absolute path".into());
        }
        let metadata=directory.metadata().map_err(|error|error.to_string())?;
        let named=read_named(path).map_err(|error|error.to_string())?;
        let identity=DirectoryIdentity::from_metadata(&metadata);
        if !metadata.is_dir()||!named.is_dir()||identity!=DirectoryIdentity::from_metadata(&named) {
            return Err("The selected agent directory changed before acceptance".into());
        }
        Ok(Self{path:path.to_owned(),identity,_directory:directory})
    }
    fn require_target(&self,path:&Path)->Result<DirectoryIdentity,String> {
        if self.path!=path {return Err("The captured agent directory belongs to another selected target".into());}
        Ok(self.identity)
    }
}

/// Metadata-only acceptance insert. The original observation was made outside
/// Registry/the writer; None remains explicitly unqualified. Never reopen the
/// current path here or in the scheduler's caller-owned firing transaction.
pub(super) fn insert_new(db:&Connection,agent:&Agent,directory:Option<&AcceptedDirectory>)->Result<(),String> {
    let identity=directory.map(|observation|observation.require_target(&agent.spec.target.directory)).transpose()?;
    insert(db,agent)?;
    let encoded=identity.as_ref().map(encode).transpose()?;
    db.execute("INSERT INTO agent_policies(agent,directory) VALUES (?1,?2) ON CONFLICT(agent) DO UPDATE SET directory=excluded.directory WHERE agent_policies.directory IS NULL",params![agent.id.to_string(),encoded]).map_err(|error|error.to_string())?;
    Ok(())
}

fn open_directory(path: &Path) -> std::io::Result<File> {
    if path == Path::new("/") {
        return std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path);
    }
    let root = crate::fs_safety::Root::open(Path::new("/"))?;
    let relative = path
        .strip_prefix("/")
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;
    let directory =
        crate::fs_safety::open_beneath(&root, relative).map_err(std::io::Error::other)?;
    if !directory.metadata()?.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotADirectory,
            "approved path is not a directory",
        ));
    }
    Ok(directory)
}

pub(super) fn all(db: &Connection) -> Result<Vec<crate::workspace_model::AgentLocation>, String> {
    let rows = db.prepare("SELECT l.agent,l.repository,l.directory,l.blocked_directory FROM agent_locations l JOIN agents a ON a.id=l.agent WHERE a.closed=0 ORDER BY l.agent").map_err(|error| error.to_string())?.query_map([], |row| Ok((row.get::<_,String>(0)?, row.get::<_,String>(1)?, row.get::<_,String>(2)?, row.get::<_,Option<String>>(3)?))).and_then(Iterator::collect::<rusqlite::Result<Vec<_>>>).map_err(|error| error.to_string())?;
    rows.into_iter()
        .map(|(agent, repository, directory, blocked)| {
            Ok(crate::workspace_model::AgentLocation {
                agent: agent
                    .parse()
                    .map_err(|error| format!("invalid agent location identity: {error}"))?,
                repository: repository.into(),
                directory: directory.into(),
                blocked_directory: blocked.map(PathBuf::from),
            })
        })
        .collect()
}

pub(super) fn read(db: &Connection, agent: &Agent) -> Result<Location, String> {
    db.query_row(
        "SELECT repository,directory,blocked_directory FROM agent_locations WHERE agent=?1",
        [agent.id.to_string()],
        |row| {
            Ok(Location {
                repository: PathBuf::from(row.get::<_, String>(0)?),
                directory: PathBuf::from(row.get::<_, String>(1)?),
                blocked_directory: row.get::<_, Option<String>>(2)?.map(PathBuf::from),
            })
        },
    )
    .optional()
    .map_err(|error| error.to_string())
    .map(|location| {
        location.unwrap_or_else(|| Location {
            repository: agent.spec.target.repository.clone(),
            directory: agent.spec.target.directory.clone(),
            blocked_directory: None,
        })
    })
}

pub(super) fn relocate(
    db: &Connection,
    agent: &Agent,
    relocation: &Relocation,
    moves: &[AgentMove],
) -> Result<bool, String> {
    let previous = read(db, agent)?;
    if previous.repository != relocation.repository {
        return Ok(false);
    }
    insert(db, agent)?;
    let candidate = relocation.mapped(
        previous
            .blocked_directory
            .as_deref()
            .unwrap_or(&previous.directory),
    );
    let approved: Option<String> = db
        .query_row(
            "SELECT directory FROM agent_policies WHERE agent=?1",
            [agent.id.to_string()],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| error.to_string())?
        .flatten();
    let approved = approved
        .map(|value| decode::<DirectoryIdentity>(&value))
        .transpose()?;
    let verified = moves
        .iter()
        .find(|movement| movement.input.agent == agent.id)
        .is_some_and(|movement| {
            movement.input.previous == previous
                && movement.input.candidate == candidate
                && movement
                    .input
                    .approved
                    .is_none_or(|expected| approved == Some(expected))
                && approved.is_some()
                && movement.identity == approved
        });
    // No new authority is needed when an external worktree did not move.
    let unchanged = previous.blocked_directory.is_none() && candidate == previous.directory;
    let (directory, blocked) = if verified || unchanged {
        (candidate, None)
    } else {
        (previous.directory, Some(candidate))
    };
    db.execute(
        "UPDATE agent_locations SET repository=?2,directory=?3,blocked_directory=?4 WHERE agent=?1",
        params![
            agent.id.to_string(),
            repository_key(&relocation.destination)?,
            repository_key(&directory)?,
            blocked.as_deref().map(repository_key).transpose()?
        ],
    )
    .map_err(|error| error.to_string())?;
    Ok(true)
}

impl Registry {
    /// Database-only snapshot. Caller releases all host/registry locks before
    /// invoking RelocationInput::verify for each candidate.
    pub(crate) fn agent_relocation_inputs(
        &self,
        relocation: &Relocation,
    ) -> Result<Vec<RelocationInput>, String> {
        let mut inputs = Vec::new();
        for agent in self.agents(None)? {
            let previous = read(&self.db, &agent)?;
            if previous.repository != relocation.repository {
                continue;
            }
            let candidate = relocation.mapped(
                previous
                    .blocked_directory
                    .as_deref()
                    .unwrap_or(&previous.directory),
            );
            inputs.push(RelocationInput {
                agent: agent.id,
                approved: self.agent_directory(agent.id)?,
                previous,
                candidate,
            });
        }
        Ok(inputs)
    }
    pub(crate) fn retained_agent_execution_directory(
        &self,
        retained: &[crate::agent_execution::HostedSession],
    ) -> Result<Option<PathBuf>, String> {
        // The existing source-lineage check is required before resolving any
        // operational path for an adopted holder.
        if self.retained_agent_directory(retained)?.is_none() {
            return Ok(None);
        }
        let Some(first) = retained.first() else {
            return Ok(None);
        };
        let record: Option<String> = self.db.query_row("SELECT record FROM agents WHERE session_id=?1 OR id=(SELECT agent FROM agent_launches WHERE session_id=?1)", [first.id().to_string()], |row| row.get(0)).optional().map_err(|error| error.to_string())?;
        record
            .map(|record| self.agent_execution_directory(decode::<Agent>(&record)?.id))
            .transpose()
    }

    pub(crate) fn agent_location(&self, id: AgentId) -> Result<Location, String> {
        read(&self.db, &self.agent(id)?)
    }

    /// A path is usable only while it still names the exact approved inode.
    /// This check is repeated by ExecutionContext after opening the directory.
    pub(crate) fn agent_execution_directory(&self, id: AgentId) -> Result<PathBuf, String> {
        let location = self.agent_location(id)?;
        if let Some(candidate) = location.blocked_directory {
            return Err(format!(
                "Agent {id}'s located directory {} does not match its approved directory identity. Close this agent and start a new one for the replacement folder.",
                candidate.display()
            ));
        }
        if let Some(approved) = self.agent_directory(id)? {
            let directory = open_directory(&location.directory).map_err(|error| {
                format!(
                    "Agent {id}'s approved directory {} is unavailable: {error}",
                    location.directory.display()
                )
            })?;
            let actual = DirectoryIdentity::from_metadata(
                &directory.metadata().map_err(|error| error.to_string())?,
            );
            if actual != approved {
                return Err(format!(
                    "Agent {id}'s directory {} was replaced; existing process authority cannot be transferred to it",
                    location.directory.display()
                ));
            }
        }
        Ok(location.directory)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        agent::Provider,
        agent_execution::{Accepted, Destination, Origin, Spec, Submit, Surface, Target},
        model::ProjectKind,
        workspace_model::{ModelEdit, views::Scope},
    };

    fn accepted(registry: &mut Registry, repository: &Path, key: &str) -> Accepted {
        registry
            .apply(&ModelEdit::OpenProject {
                checkout: None,
                primary_checkout: None,
                repository: repository.into(),
                name: key.into(),
                kind: ProjectKind::Folder,
            })
            .unwrap();
        let placement = Scope::Worktree {
            repository: repository.into(),
            path: repository.into(),
        };
        registry
            .apply(&ModelEdit::SeedView {
                scope: placement.clone(),
                panes: Vec::new(),
                closed: Vec::new(),
            })
            .unwrap();
        let observed=AcceptedDirectory::observe(repository,std::sync::Arc::new(open_directory(repository).unwrap())).unwrap();
        registry
            .submit_agent_with_directory(
                &Origin::User,
                &Submit {
                    name: None,
                    destination: Destination::New(Spec {
                        provider: Provider::Claude,
                        account: None,
                        target: Target {
                            repository: repository.into(),
                            directory: repository.into(),
                            placement: Some(placement),
                        },
                        surface: Surface::Chat,
                        model: None,
                        permission_mode: None,
                        reasoning_effort: None,
                    }),
                    prompt: "fixture only, no provider starts".into(),
                    idempotency_key: key.into(),
                },
                None,
                Some(observed),
                1,
            )
            .unwrap()
    }

    #[test]
    fn same_inode_locate_preserves_accepted_spec_and_followup_dispatch_after_restart() {
        let fixture = tempfile::tempdir().unwrap();
        let old = fixture.path().join("old");
        std::fs::create_dir(&old).unwrap();
        let path = fixture.path().join("registry.sqlite3");
        let mut registry = Registry::at(&path).unwrap();
        let accepted = accepted(&mut registry, &old, "original");
        let proof = registry
            .agent_directory(accepted.agent.id)
            .unwrap()
            .unwrap();
        let moved = fixture.path().join("moved");
        std::fs::rename(&old, &moved).unwrap();
        let relocation = Relocation {
            repository: old.clone(),
            destination: moved.clone(),
            old_root: old.clone(),
            new_root: moved.clone(),
        };
        registry
            .apply(&ModelEdit::RelocateProject { relocation })
            .unwrap();
        assert_eq!(
            registry.agent(accepted.agent.id).unwrap().spec,
            accepted.agent.spec
        );
        assert_eq!(
            registry
                .agent_location(accepted.agent.id)
                .unwrap()
                .repository,
            moved
        );
        assert_eq!(
            registry
                .agent_execution_directory(accepted.agent.id)
                .unwrap(),
            moved
        );
        registry.validate_agent_dispatch(&accepted).unwrap();
        assert!(registry.claim_agent_dispatch(&accepted, 2).unwrap());
        let started = registry
            .update_agent_run(
                accepted.run.id,
                accepted.run.revision,
                crate::agent_execution::Update::Started {
                    provider_key: "fixture".into(),
                },
                3,
            )
            .unwrap();
        registry
            .update_agent_run(
                started.id,
                started.revision,
                crate::agent_execution::Update::Finished {
                    state: crate::agent_execution::State::Succeeded,
                    result: Default::default(),
                },
                4,
            )
            .unwrap();
        let followup = registry
            .submit_agent(
                &Origin::User,
                &Submit {
                    name: None,
                    destination: Destination::Existing(accepted.agent.id),
                    prompt: "followup fixture".into(),
                    idempotency_key: "followup".into(),
                },
                5,
            )
            .unwrap();
        assert_eq!(followup.agent.spec, accepted.agent.spec);
        registry.validate_agent_dispatch(&followup).unwrap();
        assert!(registry.claim_agent_dispatch(&followup, 6).unwrap());
        drop(registry);
        let registry = Registry::at(&path).unwrap();
        assert_eq!(
            registry.agent_directory(accepted.agent.id).unwrap(),
            Some(proof)
        );
        assert_eq!(
            registry
                .agent_execution_directory(accepted.agent.id)
                .unwrap(),
            moved
        );
        assert_eq!(
            registry.agent(accepted.agent.id).unwrap().spec,
            accepted.agent.spec
        );
    }

    #[test]
    fn reused_old_path_is_a_distinct_project_and_cannot_redirect_original_agent() {
        let fixture = tempfile::tempdir().unwrap();
        let old = fixture.path().join("old");
        std::fs::create_dir(&old).unwrap();
        let mut registry = Registry::at(&fixture.path().join("registry.sqlite3")).unwrap();
        let original = accepted(&mut registry, &old, "original");
        let moved = fixture.path().join("moved");
        std::fs::rename(&old, &moved).unwrap();
        registry
            .apply(&ModelEdit::RelocateProject {
                relocation: Relocation {
                    repository: old.clone(),
                    destination: moved.clone(),
                    old_root: old.clone(),
                    new_root: moved.clone(),
                },
            })
            .unwrap();
        std::fs::create_dir(&old).unwrap();
        let replacement = accepted(&mut registry, &old, "replacement");
        assert_eq!(
            registry
                .agent_execution_directory(original.agent.id)
                .unwrap(),
            moved
        );
        assert_eq!(
            registry
                .agent_execution_directory(replacement.agent.id)
                .unwrap(),
            old
        );
        assert_ne!(
            registry.agent_directory(original.agent.id).unwrap(),
            registry.agent_directory(replacement.agent.id).unwrap()
        );
        assert!(
            super::super::execution::require_placement(
                &registry.db,
                &replacement.agent.placement,
                &moved
            )
            .is_err()
        );
        registry.validate_agent_dispatch(&original).unwrap();
        registry.validate_agent_dispatch(&replacement).unwrap();
    }

    #[test]
    fn acceptance_pins_the_callers_descriptor_and_rolls_back_on_a_racing_replacement() {
        let fixture=tempfile::tempdir().unwrap();let old=fixture.path().join("old");std::fs::create_dir(&old).unwrap();
        let mut registry=Registry::at(&fixture.path().join("registry.sqlite3")).unwrap();
        let first=accepted(&mut registry,&old,"original");
        let original=std::sync::Arc::new(open_directory(&old).unwrap());
        let moved=fixture.path().join("moved");std::fs::rename(&old,&moved).unwrap();std::fs::create_dir(&old).unwrap();
        let before=registry.model().unwrap();
        assert!(AcceptedDirectory::observe(&old,original).unwrap_err().contains("changed before acceptance"));
        assert_eq!(registry.model().unwrap(),before);
        assert!(registry.agent_execution_directory(first.agent.id).is_err(),"the original approved directory cannot become the replacement");
    }

    #[test]
    fn uncaptured_acceptance_and_scheduler_transaction_do_not_approve_an_existing_directory() {
        let fixture=tempfile::tempdir().unwrap();let path=fixture.path().join("project");std::fs::create_dir(&path).unwrap();
        let mut registry=Registry::at(&fixture.path().join("registry.sqlite3")).unwrap();
        let first=accepted(&mut registry,&path,"qualified");let mut request=registry.agent_request(first.run.id).unwrap();
        request.idempotency_key="unobserved-acceptance".into();
        let unqualified=registry.submit_agent_with_directory(&Origin::User,&request,None,None,2).unwrap();
        assert_eq!(registry.agent_directory(unqualified.agent.id).unwrap(),None,"a real existing path cannot silently upgrade an absent observation");
        assert_eq!(registry.agent_location(unqualified.agent.id).unwrap().directory,path);
        request.idempotency_key="caller-owned-scheduler-transaction".into();
        let before=registry.model().unwrap();
        let tx=registry.db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).unwrap();
        let scheduled=Registry::submit_agent_in(&tx,&Origin::User,&request,3).unwrap();
        let approved:Option<String>=tx.query_row("SELECT directory FROM agent_policies WHERE agent=?1",[scheduled.agent.id.to_string()],|row|row.get(0)).unwrap();
        assert!(approved.is_none(),"no-I/O scheduler acceptance carries no newly observed directory approval");
        assert_eq!(read(&tx,&scheduled.agent).unwrap().directory,path);
        tx.rollback().unwrap();assert_eq!(registry.model().unwrap(),before);
        assert!(registry.agent(scheduled.agent.id).is_err(),"scheduler remains inside its caller's real atomic rollback");
    }

    #[test]
    fn observed_original_policy_is_not_replaced_by_the_current_path_and_failed_acceptance_rolls_back_every_row() {
        let fixture=tempfile::tempdir().unwrap();let old=fixture.path().join("old");std::fs::create_dir(&old).unwrap();
        let mut registry=Registry::at(&fixture.path().join("registry.sqlite3")).unwrap();let first=accepted(&mut registry,&old,"first");
        let mut request=registry.agent_request(first.run.id).unwrap();request.idempotency_key="observed-before-replacement".into();
        let directory=std::sync::Arc::new(open_directory(&old).unwrap());
        let observation=AcceptedDirectory::observe(&old,directory.clone()).unwrap();let original=observation.identity;
        let moved=fixture.path().join("moved");std::fs::rename(&old,&moved).unwrap();std::fs::create_dir(&old).unwrap();
        let observed=registry.submit_agent_with_directory(&Origin::User,&request,None,Some(observation),2).unwrap();
        assert_eq!(registry.agent_directory(observed.agent.id).unwrap(),Some(original),"the TX consumes original evidence instead of reopening the replacement");
        assert!(registry.agent_execution_directory(observed.agent.id).unwrap_err().contains("replaced"),"a later named-path replacement remains visibly refused before dispatch");
        // An already-replaced name is rejected by the owning observation
        // boundary, before any writer or acceptance is entered.
        assert!(AcceptedDirectory::observe(&old,directory).is_err());
        let replacement=std::sync::Arc::new(open_directory(&old).unwrap());
        let observation=AcceptedDirectory::observe(&old,replacement.clone()).unwrap();
        let before=registry.model().unwrap();
        let counts=|registry:&Registry|["agents","agent_runs","agent_locations","agent_policies"].map(|table|registry.db.query_row(&format!("SELECT count(*) FROM {table}"),[],|row|row.get::<_,u64>(0)).unwrap());
        let original_counts=counts(&registry);request.idempotency_key="rollback-after-policy".into();
        registry.db.execute_batch("CREATE TEMP TRIGGER fail_accepted_run BEFORE INSERT ON agent_runs BEGIN SELECT RAISE(ABORT,'fixture reached run insert after location and policy'); END;").unwrap();
        let error=registry.submit_agent_with_directory(&Origin::User,&request,None,Some(observation),3).unwrap_err();
        assert!(error.contains("fixture reached run insert after location and policy"),"{error}");
        assert_eq!(counts(&registry),original_counts);assert_eq!(registry.model().unwrap(),before);
        registry.db.execute_batch("DROP TRIGGER fail_accepted_run").unwrap();
        let mut wrong=request.clone();let Destination::New(spec)=&mut wrong.destination else{unreachable!()};spec.target.directory=moved;
        let observation=AcceptedDirectory::observe(&old,replacement).unwrap();
        assert!(registry.submit_agent_with_directory(&Origin::User,&wrong,None,Some(observation),4).unwrap_err().contains("another selected target"));
        assert_eq!(counts(&registry),original_counts);
    }

    #[test]
    fn held_actual_directory_observation_does_not_hold_the_registry_writer() {
        let fixture=tempfile::tempdir().unwrap();let path=fixture.path().join("project");std::fs::create_dir(&path).unwrap();
        let database=fixture.path().join("registry.sqlite3");let mut registry=Registry::at(&database).unwrap();
        let first=accepted(&mut registry,&path,"initial");let mut request=registry.agent_request(first.run.id).unwrap();request.idempotency_key="observed-on-worker".into();
        let other=Registry::at(&database).unwrap();
        let pin=std::sync::Arc::new(open_directory(&path).unwrap());let directory=path.clone();
        let (entered,ready)=crossbeam_channel::bounded(1);let (release,released)=crossbeam_channel::bounded(1);
        let worker=std::thread::spawn(move||AcceptedDirectory::observe_with(&directory,pin,|path| {
            entered.send(()).unwrap();released.recv_timeout(std::time::Duration::from_secs(5)).unwrap();std::fs::symlink_metadata(path)
        }));
        ready.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
        let started=std::time::Instant::now();
        // Actual second SQLite connection/writer commits while the worker is
        // held at its real metadata read boundary. This is a deterministic
        // admission fixture, not a measured network-filesystem stall.
        let tx=other.db.unchecked_transaction().unwrap();
        tx.execute("UPDATE metadata SET sequence=sequence+1 WHERE key=0",[]).unwrap();tx.commit().unwrap();
        assert!(started.elapsed()<std::time::Duration::from_secs(2));
        release.send(()).unwrap();let observation=worker.join().unwrap().unwrap();
        let accepted=registry.submit_agent_with_directory(&Origin::User,&request,None,Some(observation),2).unwrap();
        assert!(registry.agent_directory(accepted.agent.id).unwrap().is_some());
    }

    #[test]
    fn prepared_inode_evidence_cannot_authorize_a_later_path_replacement() {
        let fixture = tempfile::tempdir().unwrap();
        let old = fixture.path().join("old");
        std::fs::create_dir(&old).unwrap();
        let mut registry = Registry::at(&fixture.path().join("registry.sqlite3")).unwrap();
        let accepted = accepted(&mut registry, &old, "original");
        let moved = fixture.path().join("moved");
        std::fs::rename(&old, &moved).unwrap();
        let relocation = Relocation {
            repository: old.clone(),
            destination: moved.clone(),
            old_root: old,
            new_root: moved.clone(),
        };
        let evidence = registry
            .agent_relocation_inputs(&relocation)
            .unwrap()
            .into_iter()
            .map(RelocationInput::verify)
            .collect::<Vec<_>>();
        assert!(evidence[0]._directory.is_some());
        std::fs::rename(&moved, fixture.path().join("actual-approved-inode")).unwrap();
        std::fs::create_dir(&moved).unwrap();
        registry
            .apply_with_location_moves(&ModelEdit::RelocateProject { relocation }, &[], &evidence)
            .unwrap();
        assert!(
            registry
                .agent_execution_directory(accepted.agent.id)
                .unwrap_err()
                .contains("replaced")
        );
        assert_eq!(
            registry.agent(accepted.agent.id).unwrap().spec,
            accepted.agent.spec
        );
    }

    #[test]
    fn wrong_inode_locate_retains_state_and_refuses_existing_execution() {
        let fixture = tempfile::tempdir().unwrap();
        let old = fixture.path().join("old");
        std::fs::create_dir(&old).unwrap();
        let mut registry = Registry::at(&fixture.path().join("registry.sqlite3")).unwrap();
        let accepted = accepted(&mut registry, &old, "original");
        let hidden = fixture.path().join("original-inode");
        std::fs::rename(&old, &hidden).unwrap();
        let replacement = fixture.path().join("replacement");
        std::fs::create_dir(&replacement).unwrap();
        registry
            .apply(&ModelEdit::RelocateProject {
                relocation: Relocation {
                    repository: old.clone(),
                    destination: replacement.clone(),
                    old_root: old.clone(),
                    new_root: replacement.clone(),
                },
            })
            .unwrap();
        let location = registry.agent_location(accepted.agent.id).unwrap();
        assert_eq!(location.repository, replacement);
        assert_eq!(location.directory, old);
        assert_eq!(location.blocked_directory, Some(replacement));
        assert!(
            registry
                .agent_execution_directory(accepted.agent.id)
                .unwrap_err()
                .contains("approved directory identity")
        );
        assert_eq!(
            registry.agent(accepted.agent.id).unwrap().spec,
            accepted.agent.spec
        );
        // Recreating the original spelling is never a global alias grant.
        std::fs::create_dir(&old).unwrap();
        assert!(
            registry
                .agent_execution_directory(accepted.agent.id)
                .is_err()
        );
    }
}
