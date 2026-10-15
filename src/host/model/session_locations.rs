//! Private operational locations for exact hosted sessions. Issued recipes and
//! holder recovery sources remain immutable; a logical project move cannot
//! grant an old provider authority over a replacement directory.
use super::*;
use crate::{
    agent_execution::{DirectoryIdentity, HostedSession},
    daemon::holder_registry::Process,
    model::TerminalId,
};

#[derive(Clone, Debug)]
pub(crate) struct Location {
    pub(crate) source: HostedSession,
    pub(crate) repository: Option<PathBuf>,
    pub(crate) directory: PathBuf,
    pub(crate) approved: Option<DirectoryIdentity>,
    pub(crate) process: Option<Process>,
    pub(crate) blocked_directory: Option<PathBuf>,
    pub(crate) refusal: Option<String>,
    previous_sources: Vec<[u8; 32]>,
}

#[derive(Clone, Debug)]
pub(crate) struct SessionMove {
    pub(crate) source: HostedSession,
    pub(crate) repository: PathBuf,
    pub(crate) candidate: PathBuf,
    pub(crate) proof: Result<(DirectoryIdentity, Process), String>,
}

fn encode<T: serde::Serialize>(value: &T) -> Result<String, String> {
    serde_json::to_string(value).map_err(|error| error.to_string())
}
fn decode<T: serde::de::DeserializeOwned>(value: &str) -> Result<T, String> {
    serde_json::from_str(value).map_err(|error| error.to_string())
}
fn issued_directory(source: &HostedSession) -> &Path {
    match source {
        HostedSession::Chat(session) => &session.launch.recipe.working_directory,
        HostedSession::Terminal(session) => &session.launch.working_directory,
    }
}
fn same_issued_launch(a: &HostedSession, b: &HostedSession) -> bool {
    match (a, b) {
        (HostedSession::Chat(a), HostedSession::Chat(b)) => {
            a.id == b.id
                && a.launch.provider == b.launch.provider
                && a.launch.recipe == b.launch.recipe
        }
        (HostedSession::Terminal(a), HostedSession::Terminal(b)) => {
            a.id == b.id && a.launch == b.launch
        }
        _ => false,
    }
}
fn compatible_chat_metadata(cached: &HostedSession, current: &HostedSession) -> bool {
    match (cached, current) {
        (HostedSession::Chat(cached), HostedSession::Chat(current)) => {
            cached
                .launch
                .session_id
                .as_ref()
                .is_none_or(|id| current.launch.session_id.as_ref() == Some(id))
                && cached
                    .owned_codex_listener
                    .as_ref()
                    .is_none_or(|listener| current.owned_codex_listener.as_ref() == Some(listener))
                && cached.terminal.as_ref().is_none_or(|terminal| {
                    current.terminal.as_ref().is_some_and(|current| {
                        current.id == terminal.id && current.launch == terminal.launch
                    })
                })
        }
        (HostedSession::Terminal(_), HostedSession::Terminal(_)) => true,
        _ => false,
    }
}
fn same_source(cached: &HostedSession, current: &HostedSession) -> bool {
    cached.same_process(current)
        && compatible_chat_metadata(cached, current)
        && match (cached, current) {
            (HostedSession::Chat(cached), HostedSession::Chat(current)) => {
                cached.terminal.as_ref().is_none_or(|terminal| {
                    current.terminal.as_ref().is_some_and(|current| {
                        terminal == current
                            || HostedSession::Terminal(terminal.clone())
                                .same_process(&HostedSession::Terminal(current.clone()))
                    })
                })
            }
            _ => true,
        }
}
fn source_key(source: &HostedSession) -> Result<Option<[u8; 32]>, String> {
    // Thread enrichment is compared against the current authenticated actor
    // separately. The private old source can predate its first confirmed ID.
    let value = match source {
        HostedSession::Chat(session) if session.proof.is_some() => serde_json::to_vec(&(
            "chat",
            session.id,
            &session.proof,
            session.launch.provider,
            &session.launch.recipe,
            &session.owned_codex_listener,
            session.terminal.as_ref().map(|terminal| {
                (
                    terminal.id,
                    &terminal.launch,
                    terminal.continuity.live_proof(),
                )
            }),
        )),
        HostedSession::Terminal(session) if session.continuity.live_proof().is_some() => {
            serde_json::to_vec(&(
                "terminal",
                session.id,
                session.continuity.live_proof(),
                &session.launch,
            ))
        }
        _ => return Ok(None),
    }
    .map_err(|error| error.to_string())?;
    use sha2::Digest;
    Ok(Some(sha2::Sha256::digest(&value).into()))
}
fn retain_source(location: &mut Location) -> Result<(), String> {
    let source = location.source.clone();
    retain_alias(location, &source)
}
fn retain_alias(location: &mut Location, source: &HostedSession) -> Result<(), String> {
    if let Some(key) = source_key(source)? {
        location.previous_sources.retain(|known| *known != key);
        location.previous_sources.push(key);
        if location.previous_sources.len() > 64 {
            location.previous_sources.remove(0);
        }
    }
    Ok(())
}

pub(super) fn migrate(db: &Connection) -> Result<(), String> {
    let tx = rusqlite::Transaction::new_unchecked(db, rusqlite::TransactionBehavior::Immediate)
        .map_err(|error| error.to_string())?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS session_locations (
        session TEXT PRIMARY KEY, source TEXT NOT NULL, repository TEXT,
        directory TEXT NOT NULL, approved TEXT, process TEXT,
        blocked_directory TEXT, refusal TEXT, previous_sources TEXT NOT NULL DEFAULT '[]'
    ) STRICT;",
    )
    .map_err(|error| error.to_string())?;
    tx.pragma_update(None, "user_version", 14)
        .map_err(|error| error.to_string())?;
    tx.commit().map_err(|error| error.to_string())
}

fn read(db: &Connection, id: TerminalId) -> Result<Option<Location>, String> {
    let row = db.query_row("SELECT source,repository,directory,approved,process,blocked_directory,refusal,previous_sources FROM session_locations WHERE session=?1", [id.to_string()], |row| {
        Ok((row.get::<_,String>(0)?, row.get::<_,Option<String>>(1)?, row.get::<_,String>(2)?, row.get::<_,Option<String>>(3)?, row.get::<_,Option<String>>(4)?, row.get::<_,Option<String>>(5)?, row.get::<_,Option<String>>(6)?, row.get::<_,String>(7)?))
    }).optional().map_err(|error| error.to_string())?;
    row.map(
        |(source, repository, directory, approved, process, blocked, refusal, previous)| {
            Ok(Location {
                source: decode(&source)?,
                repository: repository.map(PathBuf::from),
                directory: directory.into(),
                approved: approved.map(|value| decode(&value)).transpose()?,
                process: process.map(|value| decode(&value)).transpose()?,
                blocked_directory: blocked.map(PathBuf::from),
                refusal,
                previous_sources: decode(&previous)?,
            })
        },
    )
    .transpose()
}

fn write(db: &Connection, location: &Location) -> Result<(), String> {
    db.execute("INSERT INTO session_locations(session,source,repository,directory,approved,process,blocked_directory,refusal,previous_sources) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9) ON CONFLICT(session) DO UPDATE SET source=excluded.source,repository=excluded.repository,directory=excluded.directory,approved=excluded.approved,process=excluded.process,blocked_directory=excluded.blocked_directory,refusal=excluded.refusal,previous_sources=excluded.previous_sources", params![
        location.source.id().to_string(), encode(&location.source)?,
        location.repository.as_deref().map(repository_key).transpose()?, repository_key(&location.directory)?,
        location.approved.as_ref().map(encode).transpose()?, location.process.as_ref().map(encode).transpose()?,
        location.blocked_directory.as_deref().map(repository_key).transpose()?, location.refusal, encode(&location.previous_sources)?,
    ]).map_err(|error| error.to_string())?;
    Ok(())
}

pub(super) fn refusals(
    db: &Connection,
    views: &[crate::workspace_model::views::View],
    agents: &[crate::agent_execution::Agent],
) -> Result<Vec<crate::workspace_model::SessionLocationRefusal>, String> {
    let mut owned = std::collections::BTreeSet::new();
    for view in views {
        for pane in &view.panes {
            match &pane.kind {
                crate::model::CardKind::Terminal(session) => {
                    owned.insert(session.id);
                }
                crate::model::CardKind::Chat(session) => {
                    owned.insert(session.id);
                    if let Some(terminal) = &session.terminal {
                        owned.insert(terminal.id);
                    }
                }
                _ => {}
            }
        }
    }
    for agent in agents.iter().filter(|agent| !agent.closed) {
        if let Some(source) = &agent.session {
            owned.insert(source.id());
        }
        if let Some(console) = &agent.console {
            owned.insert(console.id);
        }
    }
    // Indexed lookups bound public status hydration to current ownership,
    // regardless of how many closed historical bindings remain private.
    let mut query = db.prepare("SELECT blocked_directory,refusal FROM session_locations WHERE session=?1 AND blocked_directory IS NOT NULL AND refusal IS NOT NULL").map_err(|error| error.to_string())?;
    let mut statuses = Vec::new();
    for session in owned {
        let status: Option<(String, String)> = query
            .query_row([session.to_string()], |row| Ok((row.get(0)?, row.get(1)?)))
            .optional()
            .map_err(|error| error.to_string())?;
        if let Some((directory, reason)) = status {
            statuses.push(crate::workspace_model::SessionLocationRefusal {
                session,
                directory: directory.into(),
                reason,
            });
        }
    }
    Ok(statuses)
}

/// Only server-prepared evidence enters this transaction. No process, actor or
/// filesystem operations run while HostState or the database writer is held.
pub(super) fn apply(db: &Connection, moves: &[SessionMove]) -> Result<(), String> {
    for movement in moves {
        let previous = read(db, movement.source.id())?;
        if let Some(previous) = &previous
            && !previous.source.same_process(&movement.source)
        {
            return Err("The hosted session changed while its project was being located; no relocation was committed".into());
        }
        let mut location = previous.unwrap_or_else(|| Location {
            source: movement.source.clone(),
            repository: None,
            directory: issued_directory(&movement.source).to_owned(),
            approved: None,
            process: None,
            blocked_directory: None,
            refusal: None,
            previous_sources: Vec::new(),
        });
        location.repository = Some(movement.repository.clone());
        match &movement.proof {
            Ok((identity, process)) => {
                if location
                    .approved
                    .is_some_and(|approved| approved != *identity)
                {
                    return Err("The approved session directory changed during Locate; no relocation was committed".into());
                }
                location.directory = movement.candidate.clone();
                if !compatible_chat_metadata(&location.source, &movement.source) {
                    return Err("The confirmed provider conversation changed during Locate; no relocation was committed".into());
                }
                location.source = movement.source.clone();
                location.approved = Some(*identity);
                location.process = Some(process.clone());
                location.blocked_directory = None;
                location.refusal = None;
            }
            Err(reason) => {
                location.blocked_directory = Some(movement.candidate.clone());
                location.refusal = Some(
                    reason
                        .chars()
                        .filter(|character| !character.is_control())
                        .take(1024)
                        .collect(),
                );
            }
        }
        write(db, &location)?;
    }
    Ok(())
}

/// A legacy agent can acquire its first directory pin only from its exact
/// already-bound hosted session. An ID, spec path, or public pane is not proof.
pub(super) fn pin_owned_agents(db: &Connection, moves: &[SessionMove]) -> Result<(), String> {
    if moves.is_empty() {
        return Ok(());
    }
    let records = db
        .prepare("SELECT record FROM agents")
        .map_err(|error| error.to_string())?
        .query_map([], |row| row.get::<_, String>(0))
        .and_then(Iterator::collect::<rusqlite::Result<Vec<_>>>)
        .map_err(|error| error.to_string())?;
    for record in records {
        let agent: crate::agent_execution::Agent = decode(&record)?;
        let Some(source) = &agent.session else {
            continue;
        };
        let Some(identity) = moves.iter().find_map(|movement| {
            source
                .same_process(&movement.source)
                .then(|| movement.proof.as_ref().ok().map(|(identity, _)| *identity))
                .flatten()
        }) else {
            continue;
        };
        db.execute("INSERT INTO agent_policies(agent,directory) VALUES (?1,?2) ON CONFLICT(agent) DO UPDATE SET directory=excluded.directory WHERE agent_policies.directory IS NULL", params![agent.id.to_string(), encode(&identity)?]).map_err(|error| error.to_string())?;
    }
    Ok(())
}

impl Registry {
    /// Only an exact hosted actor callback may advance its confirmed thread.
    /// This changes presentation metadata, never source proof, recipe or pin.
    pub(crate) fn observe_owned_session_thread(
        &mut self,
        source: &HostedSession,
    ) -> Result<(), String> {
        let HostedSession::Chat(current) = source else {
            return Ok(());
        };
        if current.launch.session_id.is_none() {
            return Ok(());
        }
        let tx = self.db.transaction().map_err(|error| error.to_string())?;
        let Some(mut location) = read(&tx, source.id())? else {
            return Ok(());
        };
        let mut comparison = location.source.clone();
        let HostedSession::Chat(previous) = &mut comparison else {
            return Err("A terminal binding cannot become a native conversation".into());
        };
        previous.launch.session_id = current.launch.session_id.clone();
        if !same_source(&comparison, source) {
            return Err("The confirmed thread update belongs to a different hosted source".into());
        }
        location.source = source.clone();
        write(&tx, &location)?;
        tx.commit().map_err(|error| error.to_string())
    }

    pub(crate) fn authenticated_session_alias(
        &self,
        cached: &HostedSession,
        current: &HostedSession,
    ) -> Result<bool, String> {
        if !compatible_chat_metadata(cached, current) {
            return Ok(false);
        }
        if same_source(cached, current) {
            return Ok(true);
        }
        if cached.id() != current.id() {
            return Ok(false);
        }
        let Some(location) = read(&self.db, current.id())? else {
            return Ok(false);
        };
        Ok(same_source(&location.source, current)
            && source_key(cached)?.is_some_and(|key| location.previous_sources.contains(&key)))
    }
    pub(crate) fn refuse_session_location(
        &mut self,
        source: &HostedSession,
        candidate: &Path,
        reason: &str,
    ) -> Result<(), String> {
        let tx = self.db.transaction().map_err(|error| error.to_string())?;
        let mut location = read(&tx, source.id())?
            .ok_or("The session directory binding disappeared during Locate")?;
        if !location.source.same_process(source) {
            return Err("The session incarnation changed during Locate".into());
        }
        location.blocked_directory = Some(candidate.to_owned());
        location.refusal = Some(
            reason
                .chars()
                .filter(|character| !character.is_control())
                .take(1024)
                .collect(),
        );
        write(&tx, &location)?;
        tx.execute("UPDATE metadata SET sequence=sequence+1 WHERE key=0", [])
            .map_err(|error| error.to_string())?;
        tx.commit().map_err(|error| error.to_string())
    }

    /// Called only after holder authentication and installation proved this
    /// exact successor. A public UUID/recipe never establishes this lineage.
    pub(crate) fn adopt_session_location(
        &mut self,
        retained: &[HostedSession],
        successor: &HostedSession,
    ) -> Result<(), String> {
        let Some(first) = retained.first() else {
            return Ok(());
        };
        let tx = self.db.transaction().map_err(|error| error.to_string())?;
        let Some(mut location) = read(&tx, first.id())? else {
            return Ok(());
        };
        if !same_issued_launch(&location.source, successor)
            || !compatible_chat_metadata(&location.source, successor)
            || successor.id() != first.id()
            || retained.iter().any(|source| source.id() != first.id())
            || !retained
                .iter()
                .any(|source| same_source(&location.source, source))
        {
            return Err(
                "Authenticated holder sources do not prove this session directory successor".into(),
            );
        }
        // A process proof can change during authenticated adoption, while the
        // original approved inode, refusal and operational spelling cannot.
        retain_source(&mut location)?;
        for source in retained {
            if same_issued_launch(&location.source, source) {
                retain_alias(&mut location, source)?;
            }
        }
        location.source = successor.clone();
        write(&tx, &location)?;
        tx.commit().map_err(|error| error.to_string())
    }

    pub(crate) fn session_location(
        &self,
        source: &HostedSession,
    ) -> Result<Option<Location>, String> {
        let location = read(&self.db, source.id())?;
        if location.as_ref().is_some_and(|location| {
            !location.source.same_process(source)
                || !compatible_chat_metadata(&location.source, source)
        }) {
            return Err(
                "The saved operational directory belongs to another session incarnation".into(),
            );
        }
        Ok(location)
    }

    pub(crate) fn bind_session_location(
        &mut self,
        source: &HostedSession,
        previous: Option<&HostedSession>,
        directory: &Path,
        approved: DirectoryIdentity,
    ) -> Result<(), String> {
        let tx = self.db.transaction().map_err(|error| error.to_string())?;
        let old = read(&tx, source.id())?;
        if old.as_ref().is_some_and(|location| {
            !same_issued_launch(&location.source, source)
                || !compatible_chat_metadata(&location.source, source)
        }) {
            return Err(
                "A session directory binding cannot change its original issued launch".into(),
            );
        }
        if old.as_ref().is_some_and(|location| {
            !location.source.same_process(source)
                && previous.is_none_or(|previous| !location.source.same_process(previous))
        }) {
            return Err("A different original session still owns this directory binding".into());
        }
        if old.as_ref().is_some_and(|location| {
            location
                .approved
                .is_some_and(|identity| identity != approved)
        }) {
            return Err("The original session directory was replaced; start a new session for the replacement folder".into());
        }
        let mut previous_sources = old
            .as_ref()
            .map(|location| location.previous_sources.clone())
            .unwrap_or_default();
        if let Some(mut old) = old.clone()
            && !old.source.same_process(source)
        {
            retain_source(&mut old)?;
            previous_sources = old.previous_sources;
        }
        write(
            &tx,
            &Location {
                source: source.clone(),
                repository: old
                    .as_ref()
                    .and_then(|location| location.repository.clone()),
                directory: directory.to_owned(),
                approved: Some(approved),
                process: None,
                blocked_directory: None,
                refusal: None,
                previous_sources,
            },
        )?;
        tx.commit().map_err(|error| error.to_string())
    }

    /// Holder authentication must supply the exact private retained sources.
    /// A matching terminal ID or filesystem path alone never restores a pin.
    pub(crate) fn retained_session_location(
        &self,
        retained: &[HostedSession],
    ) -> Result<Option<Location>, String> {
        let Some(first) = retained.first() else {
            return Ok(None);
        };
        let Some(location) = read(&self.db, first.id())? else {
            return Ok(None);
        };
        if retained.iter().any(|source| source.id() != first.id())
            || !retained
                .iter()
                .any(|source| same_source(&location.source, source))
        {
            return Err(
                "Retained provider sources do not authenticate the saved directory binding".into(),
            );
        }
        if let Some(reason) = &location.refusal {
            return Err(reason.clone());
        }
        if location.approved.is_none() {
            return Err("The retained session has no proven directory identity; locate its original provider or close it explicitly".into());
        }
        Ok(Some(location))
    }

    /// Read-only attribution for an authenticated orphan census. A blocked
    /// directory still belongs to its logical project; this cannot approve a
    /// directory, adopt a session, or authorize process teardown.
    pub(crate) fn retained_session_repository(
        &self,
        retained: &[HostedSession],
    ) -> Result<Option<PathBuf>, String> {
        let Some(first) = retained.first() else { return Ok(None) };
        let Some(location) = read(&self.db, first.id())? else { return Ok(None) };
        if retained.iter().any(|source| source.id() != first.id()) {
            return Err("Orphan sources contain different session identities".into());
        }
        for source in retained {
            if same_source(&location.source, source)
                || source_key(source)?.is_some_and(|key| location.previous_sources.contains(&key))
            {
                return Ok(location.repository);
            }
        }
        Err("Orphan sources do not authenticate the saved logical project".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        agent::Provider,
        chat::{Launch, Session},
        model::{HostIncarnation, HostSessionId, HostSessionProof, LaunchRecipe, ProjectKind},
        project_recovery::Relocation,
    };

    fn source(directory: &Path) -> HostedSession {
        HostedSession::Chat(Session {
            id: TerminalId::new(),
            launch: Launch {
                provider: Provider::Omp,
                recipe: LaunchRecipe {
                    program: "/bin/cat".into(),
                    arguments: Vec::new(),
                    working_directory: directory.into(),
                    environment: Default::default(),
                    unset_environment: Default::default(),
                },
                session_id: None,
            },
            proof: Some(HostSessionProof {
                host_session_id: HostSessionId::new("source"),
                boot_id: crate::daemon::read_boot_id().unwrap(),
                host_incarnation: HostIncarnation::new("original"),
            }),
            terminal: None,
            native_continuity: Default::default(),
            owned_codex_listener: None,
        })
    }

    #[test]
    fn changed_session_incarnation_rolls_back_the_entire_project_location() {
        let fixture = tempfile::tempdir().unwrap();
        let old = fixture.path().join("old");
        let moved = fixture.path().join("moved");
        std::fs::create_dir(&old).unwrap();
        std::fs::create_dir(&moved).unwrap();
        let mut registry = Registry::at(&fixture.path().join("workspace.sqlite3")).unwrap();
        registry
            .apply(&ModelEdit::OpenProject {
                checkout: None,
                primary_checkout: None,
                repository: old.clone(),
                name: "Original".into(),
                kind: ProjectKind::Folder,
            })
            .unwrap();
        let source = source(&old);
        let identity = DirectoryIdentity::from_metadata(&std::fs::metadata(&old).unwrap());
        registry
            .bind_session_location(&source, None, &old, identity)
            .unwrap();
        let before = registry.model().unwrap();
        let mut changed = source.clone();
        let HostedSession::Chat(session) = &mut changed else {
            unreachable!()
        };
        session.proof.as_mut().unwrap().host_incarnation = HostIncarnation::new("unrelated");
        let movement = SessionMove {
            source: changed,
            repository: moved.clone(),
            candidate: moved.clone(),
            proof: Err("Unproved provider".into()),
        };
        let edit = ModelEdit::RelocateProject {
            relocation: Relocation {
                repository: old.clone(),
                destination: moved.clone(),
                old_root: old.clone(),
                new_root: moved,
            },
        };
        assert!(
            registry
                .apply_with_session_moves(&edit, &[movement])
                .unwrap_err()
                .contains("session changed")
        );
        assert_eq!(registry.model().unwrap(), before);
        assert_eq!(
            registry
                .session_location(&source)
                .unwrap()
                .unwrap()
                .directory,
            old
        );
    }

    #[test]
    fn only_exact_authenticated_retained_sources_adopt_directory_bindings() {
        let fixture = tempfile::tempdir().unwrap();
        let mut registry = Registry::at(&fixture.path().join("workspace.sqlite3")).unwrap();
        let source = source(fixture.path());
        let identity =
            DirectoryIdentity::from_metadata(&std::fs::metadata(fixture.path()).unwrap());
        registry
            .bind_session_location(&source, None, fixture.path(), identity)
            .unwrap();
        let mut successor = source.clone();
        let HostedSession::Chat(session) = &mut successor else {
            unreachable!()
        };
        session.proof.as_mut().unwrap().host_incarnation = HostIncarnation::new("successor");
        let mut forged = source.clone();
        let HostedSession::Chat(session) = &mut forged else {
            unreachable!()
        };
        session
            .launch
            .recipe
            .arguments
            .push("--different-source".into());
        assert!(
            registry
                .retained_session_location(&[forged.clone()])
                .is_err()
        );
        assert!(
            registry
                .adopt_session_location(&[forged], &successor)
                .is_err()
        );
        registry
            .adopt_session_location(&[source.clone()], &successor)
            .unwrap();
        let saved = registry.session_location(&successor).unwrap().unwrap();
        assert_eq!(saved.approved, Some(identity));
        assert_eq!(saved.directory, fixture.path());
        assert!(registry.session_location(&source).is_err());
        let mut replaced = identity;
        replaced.inode = replaced.inode.wrapping_add(1);
        assert!(
            registry
                .bind_session_location(&successor, None, fixture.path(), replaced)
                .unwrap_err()
                .contains("replaced")
        );
    }

    #[test]
    fn aliases_never_replace_a_known_provider_thread() {
        let fixture = tempfile::tempdir().unwrap();
        let mut registry = Registry::at(&fixture.path().join("workspace.sqlite3")).unwrap();
        let mut source = source(fixture.path());
        let HostedSession::Chat(session) = &mut source else {
            unreachable!()
        };
        session.launch.session_id = Some("22222222-0000-4000-8000-000000000001".into());
        let identity =
            DirectoryIdentity::from_metadata(&std::fs::metadata(fixture.path()).unwrap());
        registry
            .bind_session_location(&source, None, fixture.path(), identity)
            .unwrap();
        let mut successor = source.clone();
        let HostedSession::Chat(session) = &mut successor else {
            unreachable!()
        };
        session.proof.as_mut().unwrap().host_incarnation = HostIncarnation::new("successor");
        registry
            .adopt_session_location(&[source.clone()], &successor)
            .unwrap();
        assert!(
            registry
                .authenticated_session_alias(&source, &successor)
                .unwrap()
        );
        let mut different_thread = source.clone();
        let HostedSession::Chat(session) = &mut different_thread else {
            unreachable!()
        };
        session.launch.session_id = Some("33333333-0000-4000-8000-000000000002".into());
        assert!(
            !registry
                .authenticated_session_alias(&different_thread, &successor)
                .unwrap()
        );
        assert!(
            registry
                .adopt_session_location(&[successor.clone()], &different_thread)
                .is_err()
        );
        let mut current_wrong_thread = successor.clone();
        let HostedSession::Chat(session) = &mut current_wrong_thread else {
            unreachable!()
        };
        session.launch.session_id = Some("44444444-0000-4000-8000-000000000003".into());
        assert!(
            !registry
                .authenticated_session_alias(&source, &current_wrong_thread)
                .unwrap()
        );
    }

    #[test]
    fn refusals_preserve_approved_authority_and_publish_bounded_private_free_status() {
        let fixture = tempfile::tempdir().unwrap();
        let mut registry = Registry::at(&fixture.path().join("workspace.sqlite3")).unwrap();
        let source = source(fixture.path());
        let identity =
            DirectoryIdentity::from_metadata(&std::fs::metadata(fixture.path()).unwrap());
        registry
            .bind_session_location(&source, None, fixture.path(), identity)
            .unwrap();
        registry
            .apply(&ModelEdit::OpenProject {
                repository: fixture.path().into(),
                checkout: None,
                primary_checkout: None,
                name: "Owned".into(),
                kind: ProjectKind::Folder,
            })
            .unwrap();
        let scope = crate::workspace_model::views::Scope::Worktree {
            repository: fixture.path().into(),
            path: fixture.path().into(),
        };
        let card = crate::model::CardId::new();
        let HostedSession::Chat(chat) = &source else {
            unreachable!()
        };
        registry
            .apply(&ModelEdit::SeedView {
                scope: scope.clone(),
                panes: vec![crate::workspace_model::views::Pane {
                    id: card,
                    title: "Owned".into(),
                    user_title: None,
                    pending_terminal_start: false,
                    width_fraction: 0.5,
                    kind: crate::model::CardKind::Chat(chat.clone()),
                    scope: Some(fixture.path().into()),
                }],
                closed: vec![],
            })
            .unwrap();
        let hidden = self::source(fixture.path());
        registry
            .bind_session_location(&hidden, None, fixture.path(), identity)
            .unwrap();
        registry
            .refuse_session_location(
                &hidden,
                &fixture.path().join("closed"),
                "Private closed history",
            )
            .unwrap();
        let candidate = fixture.path().join("replacement");
        registry
            .refuse_session_location(
                &source,
                &candidate,
                &format!("Close the original provider\n{}", "x".repeat(4000)),
            )
            .unwrap();
        let saved = registry.session_location(&source).unwrap().unwrap();
        assert_eq!(saved.directory, fixture.path());
        assert_eq!(saved.approved, Some(identity));
        let status = registry.model().unwrap().session_location_refusals;
        assert_eq!(status.len(), 1);
        assert_eq!(status[0].session, source.id());
        assert_eq!(status[0].reason.chars().count(), 1024);
        assert!(!status[0].reason.chars().any(char::is_control));
        let public = serde_json::to_string(&status).unwrap();
        for private in [
            "proof",
            "approved",
            "device",
            "inode",
            "source",
            "process",
            "environment",
        ] {
            assert!(!public.contains(private));
        }
        assert!(
            registry
                .retained_session_location(&[source.clone()])
                .is_err()
        );
        let expected_revision = registry.view_revision(&scope).unwrap();
        registry
            .apply(&ModelEdit::ChangeView {
                scope,
                expected_revision,
                operation: uuid::Uuid::new_v4(),
                change: crate::workspace_model::views::Change::Remove { id: card },
            })
            .unwrap();
        assert!(
            registry
                .model()
                .unwrap()
                .session_location_refusals
                .is_empty(),
            "closed panes and unrelated history are not published"
        );
        assert!(
            registry
                .session_location(&source)
                .unwrap()
                .unwrap()
                .refusal
                .is_some(),
            "closing a pane preserves its private recovery binding"
        );
        assert!(
            registry
                .session_location(&hidden)
                .unwrap()
                .unwrap()
                .refusal
                .is_some()
        );
        registry
            .bind_session_location(&source, None, fixture.path(), identity)
            .unwrap();
        assert!(
            registry
                .session_location(&source)
                .unwrap()
                .unwrap()
                .refusal
                .is_none()
        );
    }

    #[test]
    fn exact_bound_legacy_agent_acquires_its_first_policy_from_owned_process_proof() {
        use crate::agent_execution::{Destination, Origin, Spec, Submit, Surface, Target};
        let fixture = tempfile::tempdir().unwrap();
        let old = fixture.path().join("old");
        let moved = fixture.path().join("moved");
        std::fs::create_dir(&old).unwrap();
        let mut registry = Registry::at(&fixture.path().join("workspace.sqlite3")).unwrap();
        registry
            .apply(&ModelEdit::OpenProject {
                checkout: None,
                primary_checkout: None,
                repository: old.clone(),
                name: "Legacy".into(),
                kind: ProjectKind::Folder,
            })
            .unwrap();
        let placement = crate::workspace_model::views::Scope::Worktree {
            repository: old.clone(),
            path: old.clone(),
        };
        registry
            .apply(&ModelEdit::SeedView {
                scope: placement.clone(),
                panes: Vec::new(),
                closed: Vec::new(),
            })
            .unwrap();
        let accepted = registry
            .submit_agent(
                &Origin::User,
                &Submit {
                    name: None,
                    destination: Destination::New(Spec {
                        provider: Provider::Omp,
                        account: None,
                        target: Target {
                            repository: old.clone(),
                            directory: old.clone(),
                            placement: Some(placement),
                        },
                        surface: Surface::Chat,
                        model: None,
                        permission_mode: None,
                        reasoning_effort: None,
                    }),
                    prompt: "fixture, no real provider".into(),
                    idempotency_key: "legacy-locate".into(),
                },
                1,
            )
            .unwrap();
        let source = source(&old);
        registry.claim_agent_dispatch(&accepted, 2).unwrap();
        registry
            .reserve_agent_launch(accepted.agent.id, &source)
            .unwrap();
        registry
            .bind_agent(accepted.agent.id, None, source.clone())
            .unwrap();
        registry
            .db
            .execute(
                "UPDATE agent_policies SET directory=NULL WHERE agent=?1",
                [accepted.agent.id.to_string()],
            )
            .unwrap();
        assert!(
            registry
                .agent_directory(accepted.agent.id)
                .unwrap()
                .is_none()
        );
        struct Running(std::process::Child);
        impl Drop for Running {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let child = Running(
            std::process::Command::new("/bin/sleep")
                .arg("30")
                .current_dir(&old)
                .spawn()
                .unwrap(),
        );
        let process = Process::read(child.0.id() as i32).unwrap();
        std::fs::rename(&old, &moved).unwrap();
        let proof =
            crate::chat::runtime::DirectoryProof::capture_owned_process(&process, &moved, None)
                .unwrap();
        let movement = SessionMove {
            source: source.clone(),
            repository: moved.clone(),
            candidate: moved.clone(),
            proof: Ok((proof.identity, proof.process)),
        };
        registry
            .apply_with_session_moves(
                &ModelEdit::RelocateProject {
                    relocation: Relocation {
                        repository: old.clone(),
                        destination: moved.clone(),
                        old_root: old,
                        new_root: moved.clone(),
                    },
                },
                &[movement],
            )
            .unwrap();
        assert_eq!(
            registry.agent_directory(accepted.agent.id).unwrap(),
            Some(proof.identity)
        );
        assert_eq!(
            registry
                .agent_execution_directory(accepted.agent.id)
                .unwrap(),
            moved
        );
        let agent = registry.agent(accepted.agent.id).unwrap();
        assert_eq!(agent.spec, accepted.agent.spec);
        assert_eq!(agent.session, Some(source));
        assert!(
            registry
                .agent_location(agent.id)
                .unwrap()
                .blocked_directory
                .is_none()
        );
    }
}
