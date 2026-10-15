//! Worktree consent is scoped to resident actors, including detached windows.
//! HostLock protects only the small census of handles and directory metadata;
//! actor commands, process inspection and durable retirement happen outside it.
use super::state::SharedHost;
use crate::{
    agent::ActivityState,
    agent_execution::HostedSession,
    chat,
    model::{TerminalId, TerminalSession},
    repository::{CancellationToken, RepositoryService},
    terminal::TerminalHandle,
    worktree_control::{BusyAgent, Reply, SessionIdentity, SessionStopConsent},
};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, path::{Path, PathBuf}};

struct Terminal {
    instance: uuid::Uuid,
    session: TerminalSession,
    handle: TerminalHandle,
    revision: u64,
}
struct Native {
    execution_instance: uuid::Uuid,
    session: chat::Session,
    handle: chat::runtime::Handle,
    observation: chat::runtime::CloseObservation,
}
struct Actor {
    token: BusyAgent,
    busy: bool,
    terminal: Option<Terminal>,
    native: Option<Native>,
    starting: bool,
}
type Directory = (std::sync::Arc<std::fs::File>, crate::agent_execution::DirectoryIdentity);
struct NativeCandidate {
    execution_instance: uuid::Uuid,
    session: chat::Session,
    handle: chat::runtime::Handle,
    pair_valid: bool,
    paired_live: bool,
    paired_directory: Option<Option<Directory>>,
}
struct Candidates {
    terminals: Vec<(uuid::Uuid, TerminalSession, TerminalHandle, Option<Directory>)>,
    natives: Vec<NativeCandidate>,
    starting: Vec<(TerminalId, uuid::Uuid, PathBuf)>,
}

/// Validate an exact registered checkout; a subdirectory or another repository
/// must never become authority to close a parent's actors.
fn selected_worktree(repository: &Path, path: &Path) -> Result<PathBuf, String> {
    let service = RepositoryService::system("git");
    let cancel = CancellationToken::new();
    let path = path.canonicalize().map_err(|error| error.to_string())?;
    let common = service.common_dir(repository, &cancel).map_err(|error| error.to_string())?;
    if service.common_dir(&path, &cancel).map_err(|error| error.to_string())? != common {
        return Err("The selected worktree belongs to another repository".into());
    }
    if !service.list_worktrees(repository, &cancel).map_err(|error| error.to_string())?
        .iter().any(|record| !record.bare && record.path.canonicalize().ok().as_ref() == Some(&path)) {
        return Err("Select an exact registered worktree root before closing its sessions".into());
    }
    Ok(path)
}
fn scope_directory(directory: &Path) -> Result<PathBuf, String> {
    if !directory.is_absolute() {
        return Err("A session has no absolute attributable working directory".into());
    }
    // A removed suffix does not erase physical symlinks in its existing
    // ancestors. Resolve those ancestors first, then traverse each suffix
    // component so `..` can reveal another existing symlink correctly.
    for ancestor in directory.ancestors() {
        let mut resolved = match ancestor.canonicalize() {
            Ok(path) => path,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(format!("A session directory cannot be attributed safely: {error}")),
        };
        let suffix = directory.strip_prefix(ancestor).expect("directory ancestor");
        for component in suffix.components() {
            match component {
                std::path::Component::CurDir => {},
                std::path::Component::ParentDir => { resolved.pop(); },
                std::path::Component::Normal(part) => {
                    let next = resolved.join(part);
                    match next.canonicalize() {
                        Ok(path) => resolved = path,
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                            match std::fs::symlink_metadata(&next) {
                                Err(error) if error.kind() == std::io::ErrorKind::NotFound => resolved = next,
                                // A dangling alias, permission failure or a
                                // concurrent replacement has no safe scope.
                                _ => return Err("A session directory alias is unresolved; settle it before closing the worktree".into()),
                            }
                        }
                        Err(error) => return Err(format!("A session directory cannot be attributed safely: {error}")),
                    }
                }
                _ => return Err("A session directory has an invalid relative suffix".into()),
            }
        }
        return Ok(resolved);
    }
    Err("A session directory has no resolvable ancestor".into())
}
fn inside(directory: &Path, path: &Path) -> Result<bool, String> {
    Ok(scope_directory(directory)?.starts_with(path))
}

fn admitted_scope(directory: &Path, identity: crate::agent_execution::DirectoryIdentity, selected: &Path) -> Result<bool, String> {
    if !directory.is_absolute() { return Err("An actor has no authoritative physical working directory".into()); }
    let physical = directory.canonicalize().map_err(|error| format!("An actor's admitted directory moved or disappeared: {error}"))?;
    let metadata = std::fs::symlink_metadata(&physical).map_err(|error| error.to_string())?;
    if !metadata.is_dir() || crate::agent_execution::DirectoryIdentity::from_metadata(&metadata) != identity {
        return Err("An actor's admitted directory changed identity; its process was preserved".into());
    }
    Ok(physical.starts_with(selected))
}
fn terminal_scope(directory: &std::fs::File, identity: crate::agent_execution::DirectoryIdentity, selected: &Path) -> Result<bool, String> {
    use std::os::fd::AsRawFd;
    // The retained descriptor follows same-inode renames without ever
    // reinterpreting a mutable launch alias. This I/O is outside HostLock.
    let physical = std::fs::read_link(format!("/proc/self/fd/{}", directory.as_raw_fd())).map_err(|error| error.to_string())?;
    if !physical.is_absolute() { return Err("A terminal directory has no physical attribution".into()); }
    if !physical.starts_with(selected) { return Ok(false); }
    admitted_scope(&physical, identity, selected)
}
fn candidates(host: &SharedHost) -> Result<Candidates, String> {
    let state = host.lock().unwrap_or_else(|p| p.into_inner());
    if state.shutting_down || state.has_unscoped_spawn() {
        return Err("A daemon transition has not established an attributable working directory; refresh after it settles".into());
    }
    let terminals = state.terminals.values().map(|record| (
        record.handle.execution_instance(), record.session.clone(), record.handle.clone(), record.handle.admitted_directory(),
    )).collect();
    let natives = state.chats.values().map(|record| {
        let pair_valid = match (record.paired_terminal.as_ref(), state.terminals.get(&record.session.id)) {
            (Some(expected), Some(terminal)) => terminal.handle.same_worker(expected)
                && record.session.terminal.as_ref().is_some_and(|source| super::chat::matches_paired_terminal(&terminal.session, source, state.proof())),
            // A retained handle can keep an orphaned paired worker alive even
            // after its inventory entry disappeared. Never omit that owner.
            (Some(expected), None) => expected.is_completed(),
            (None, Some(_)) => false,
            (None, None) => true,
        };
        NativeCandidate {
            execution_instance: record.execution_instance, session: record.session.clone(), handle: record.handle.clone(), pair_valid,
            paired_live: record.paired_terminal.as_ref().is_some_and(|handle| !handle.is_completed()),
            paired_directory: record.paired_terminal.as_ref().map(|handle| handle.admitted_directory()),
        }
    }).collect();
    let mut starting: Vec<_> = state.spawning_scopes.iter().map(|(id, (instance, path))| (*id, *instance, path.clone())).collect();
    starting.extend(state.chat_spawning.iter().map(|(id, session)| (*id, id.as_uuid(), session.launch.recipe.working_directory.clone())));
    starting.extend(state.execution_startups.values().map(|(session, _)| {
        let directory = match session { HostedSession::Chat(session) => &session.launch.recipe.working_directory, HostedSession::Terminal(session) => &session.launch.working_directory };
        (session.id(), session.id().as_uuid(), directory.clone())
    }));
    starting.extend(state.chat_handoffs.iter().map(|(id, receipt)| (*id, id.as_uuid(), receipt.replacement().launch.working_directory.clone())));
    starting.extend(state.native_closing.iter().map(|(id, pending)| (*id, id.as_uuid(), pending.working_directory().to_owned())));
    Ok(Candidates { terminals, natives, starting })
}
fn census(host: &SharedHost, path: &Path) -> Result<Vec<Actor>, String> {
    let candidates = candidates(host)?;
    let mut actors = BTreeMap::new();
    for (instance, session, handle, directory) in candidates.terminals {
        if directory.is_none() && handle.is_completed() { continue; }
        let (directory, identity) = directory.ok_or_else(|| format!("Terminal {} has no authoritative admitted physical directory; close it explicitly before worktree removal", session.id))?;
        // Operational/launch aliases may expand removal protection, but cannot
        // newly authorize closing an actor admitted in another physical scope.
        if !terminal_scope(&directory, identity, path)? { continue; }
        let (revision, busy) = handle.close_observation()?;
        let token = BusyAgent { session: session.id, instance, close_revision: Some(revision), turn_generation: None, session_instance: instance };
        actors.insert(session.id, Actor { token, busy, terminal: Some(Terminal { instance, session, handle, revision }), native: None, starting: false });
    }
    for NativeCandidate { execution_instance, session, handle, pair_valid, paired_live, paired_directory } in candidates.natives {
        let observation = handle.close_observation();
        // History readers and fully completed actors without observed scope
        // own no provider execution. Preserve their records instead of making
        // an unrelated historical card block every worktree's live census.
        // A retained console is independent ownership and must not disappear.
        if !observation.owns_work && observation.directory_identity.is_none() && !paired_live { continue; }
        let identity = observation.directory_identity.ok_or_else(|| format!("Native session {} has no authoritative physical scope; reconnect or explicitly close it before worktree removal", session.id))?;
        let native_inside = admitted_scope(&observation.working_directory, identity, path)?;
        if let Some(paired_directory) = paired_directory {
            let (directory, identity) = paired_directory.ok_or("A paired terminal has no authoritative physical scope; its actors were preserved")?;
            let terminal_inside = terminal_scope(&directory, identity, path)?;
            if native_inside != terminal_inside {
                return Err("A native actor and its console span different worktree scopes; close them explicitly before removing either scope".into());
            }
        }
        if !native_inside { continue; }
        if !pair_valid { return Err("A native actor has an unverified paired terminal; settle its ownership before closing this worktree".into()); }
        let mut digest = Sha256::new();
        digest.update(observation.actor_id.as_bytes()); digest.update(execution_instance.as_bytes());
        let hash = digest.finalize();
        let mut instance = uuid::Uuid::from_bytes(hash[..16].try_into().expect("SHA-256 prefix"));
        let actor = actors.entry(session.id).or_insert_with(|| Actor {
            token: BusyAgent { session: session.id, instance, close_revision: None, turn_generation: None, session_instance: instance },
            busy: false, terminal: None, native: None, starting: false,
        });
        let mut session_instance = instance;
        if let Some(terminal) = &actor.terminal {
            let mut lifetime = Sha256::new();
            lifetime.update(instance.as_bytes()); lifetime.update(terminal.instance.as_bytes());
            session_instance = uuid::Uuid::from_bytes(lifetime.finalize()[..16].try_into().expect("SHA-256 prefix"));
            // The console is one logical actor, but replacing it or admitting
            // input must invalidate the native actor's composite consent too.
            let mut digest = Sha256::new();
            digest.update(instance.as_bytes()); digest.update(terminal.instance.as_bytes()); digest.update(terminal.revision.to_le_bytes());
            let hash = digest.finalize();
            instance = uuid::Uuid::from_bytes(hash[..16].try_into().expect("SHA-256 prefix"));
        }
        actor.token = BusyAgent { session: session.id, instance, close_revision: Some(observation.close_revision), turn_generation: Some(observation.turn_generation), session_instance };
        actor.busy |= observation.owns_work && (!observation.ready || matches!(observation.activity, ActivityState::Working | ActivityState::Waiting | ActivityState::Unknown));
        actor.native = Some(Native { execution_instance, session, handle, observation });
    }
    for (id, instance, directory) in candidates.starting {
        if !inside(&directory, path)? { continue; }
        let actor = actors.entry(id).or_insert_with(|| Actor {
            token: BusyAgent { session: id, instance, close_revision: None, turn_generation: None, session_instance: instance },
            busy: true, terminal: None, native: None, starting: true,
        });
        actor.starting = true;
        actor.busy = true;
        actor.token.close_revision = None;
    }
    Ok(actors.into_values().collect())
}
fn tokens(actors: &[Actor], busy_only: bool) -> Vec<BusyAgent> {
    actors.iter().filter(|actor| !busy_only || actor.busy).map(|actor| actor.token.clone()).collect()
}
fn validate_tokens(agents: &[BusyAgent], census: &[BusyAgent]) -> Result<(), String> {
    let ordered = |tokens: &[BusyAgent]| tokens.windows(2).all(|pair| pair[0].session < pair[1].session);
    if !ordered(agents) || !ordered(census) || agents.iter().any(|agent| !census.contains(agent)) {
        return Err("Close consent must contain a sorted, unique actor roster and its exact busy subset".into());
    }
    Ok(())
}
pub(super) fn busy(host: &SharedHost, repository: &Path, path: &Path) -> Result<Reply, String> {
    let path = selected_worktree(repository, path)?;
    let actors = census(host, &path)?;
    Ok(Reply::BusySessions { agents: tokens(&actors, true), census: tokens(&actors, false) })
}

/// The caller holds exclusive control admission through final census. Every
/// actor also compares its own admitted-work revision at its close boundary.
pub(super) fn close(host: &SharedHost, repository: &Path, path: &Path, agents: &[BusyAgent], consent: &[BusyAgent]) -> Result<Reply, String> {
    validate_tokens(agents, consent)?;
    let path = selected_worktree(repository, path)?;
    let actors = census(host, &path)?;
    if tokens(&actors, true) != agents || tokens(&actors, false) != consent {
        return Err("The worktree's actor roster or work changed after consent; refresh and review it again".into());
    }
    if actors.iter().any(|actor| actor.starting) {
        return Err("A session is starting or changing ownership; wait for it to settle and review fresh consent".into());
    }
    for actor in actors {
        // Preserve a closed record until durable source-matched retirement
        // succeeds; a failed database transaction remains safely retryable.
        if let Some(native) = &actor.native {
            {
                let state = host.lock().unwrap_or_else(|p| p.into_inner());
                if state.chats.get(&actor.token.session).is_none_or(|record| record.execution_instance != native.execution_instance || !record.handle.same_actor(&native.handle) || !HostedSession::Chat(record.session.clone()).same_process(&HostedSession::Chat(native.session.clone()))) {
                    return Err("The native actor changed before close; refresh the worktree census".into());
                }
            }
            native.handle.close_if_revision(native.observation.close_revision)?;
        }
        if let Some(terminal) = &actor.terminal {
            {
                let state = host.lock().unwrap_or_else(|p| p.into_inner());
                if state.terminals.get(&actor.token.session).is_none_or(|record| record.handle.execution_instance() != terminal.instance || !record.handle.same_worker(&terminal.handle)) {
                    return Err("The terminal actor changed before close; refresh the worktree census".into());
                }
            }
            terminal.handle.close_if_revision(terminal.revision)?;
        }
        let source = if let Some(native) = &actor.native { HostedSession::Chat(native.session.clone()) }
            else { HostedSession::Terminal(actor.terminal.as_ref().expect("resident actor").session.clone()) };
        super::agent_execution::retired_hosted(host, &source)?;
        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(native) = &actor.native {
            if state.chats.get(&actor.token.session).is_none_or(|record| record.execution_instance != native.execution_instance || !record.handle.same_actor(&native.handle) || !HostedSession::Chat(record.session.clone()).same_process(&HostedSession::Chat(native.session.clone()))) {
                return Err("The native actor changed during close; its replacement was preserved".into());
            }
            state.chats.remove(&actor.token.session);
            if let Some(notifications) = &state.notifications { notifications.retire(actor.token.session); }
            state.inventory_generation += 1;
        }
        if let Some(terminal) = &actor.terminal {
            if state.terminals.get(&actor.token.session).is_none_or(|record| record.handle.execution_instance() != terminal.instance || !record.handle.same_worker(&terminal.handle)) {
                return Err("The terminal changed during close; its replacement was preserved".into());
            }
            state.retire_terminal(actor.token.session);
        }
    }
    if !census(host, &path)?.is_empty() { return Err("Another actor appeared during close; refresh the remaining worktree census".into()); }
    Ok(Reply::SessionsClosed { path })
}

/// All exact actors are fenced while control admission is excluded. Cleanup
/// deliberately runs after that gate is released; records remain on failure.
pub(super) struct PendingStop {
    path: PathBuf,
    actors: Vec<Actor>,
    terminals: Vec<crate::terminal::TerminalSessionStop>,
    deadline: std::time::Instant,
}
fn session_identities(actors: &[Actor]) -> Vec<SessionIdentity> {
    actors.iter().map(|actor| SessionIdentity {session:actor.token.session,instance:actor.token.session_instance}).collect()
}
fn validate_stop_consent(consent: &SessionStopConsent) -> Result<(), String> {
    if !consent.sessions.windows(2).all(|pair|pair[0].session < pair[1].session) {
        return Err("Session stop consent must contain a sorted, unique full session roster".into());
    }
    Ok(())
}
fn source_matches(host:&SharedHost, actor:&Actor) -> Result<(),String> {
    let state=host.lock().unwrap_or_else(|p|p.into_inner());
    if let Some(native)=&actor.native {
        if state.chats.get(&actor.token.session).is_none_or(|record|record.execution_instance!=native.execution_instance || !record.handle.same_actor(&native.handle) || !HostedSession::Chat(record.session.clone()).same_process(&HostedSession::Chat(native.session.clone()))) {
            return Err("The native session was replaced; its replacement was preserved".into());
        }
    }
    if let Some(terminal)=&actor.terminal {
        if state.terminals.get(&actor.token.session).is_none_or(|record|record.handle.execution_instance()!=terminal.instance || !record.handle.same_worker(&terminal.handle)) {
            return Err("The terminal session was replaced; its replacement was preserved".into());
        }
    }
    Ok(())
}
/// Caller holds exclusive control admission, never HostLock across actor ACKs.
pub(super) fn prepare_stop(host:&SharedHost, repository:&Path, path:&Path, consent:&SessionStopConsent)->Result<PendingStop,String> {
    validate_stop_consent(consent)?;
    let path=selected_worktree(repository,path)?;
    let actors=census(host,&path)?;
    if session_identities(&actors)!=consent.sessions {
        return Err("The worktree session roster changed after consent; refresh and review it again".into());
    }
    if actors.iter().any(|actor|actor.starting) {
        return Err("A session is starting or changing ownership; wait for it to settle before stopping sessions".into());
    }
    let deadline=std::time::Instant::now()+std::time::Duration::from_secs(10);
    let mut terminals=Vec::new();
    for actor in &actors {
        source_matches(host,actor)?;
        // Fence the console first: input and terminal-to-agent ownership
        // transitions must be excluded before admitting the native stop.
        if let Some(terminal)=&actor.terminal {
            terminals.push(terminal.handle.fence_session_stop(terminal.instance,deadline)?);
        }
        if let Some(native)=&actor.native {
            native.handle.fence_session_stop_until(native.observation.actor_id,deadline)?;
        }
    }
    Ok(PendingStop {path,actors,terminals,deadline})
}
impl PendingStop {
    /// No HostLock or global control admission may be held by this caller.
    pub(super) fn cleanup(&mut self)->Result<(),String> {
        for actor in &self.actors {
            if let Some(native)=&actor.native { native.handle.close_until(self.deadline)?; }
        }
        for terminal in self.terminals.drain(..) { terminal.finish(self.deadline)?; }
        Ok(())
    }
    /// Caller reacquires exclusive admission after cleanup. A replacement is
    /// never retired on the strength of an older actor's successful cleanup.
    pub(super) fn commit(self,host:&SharedHost)->Result<Reply,String> {
        for actor in &self.actors { source_matches(host,actor)?; }
        for actor in &self.actors {
            let source=if let Some(native)=&actor.native {HostedSession::Chat(native.session.clone())}
                else {HostedSession::Terminal(actor.terminal.as_ref().expect("resident actor").session.clone())};
            super::agent_execution::retired_hosted(host,&source)?;
            let mut state=host.lock().unwrap_or_else(|p|p.into_inner());
            if let Some(native)=&actor.native {
                if state.chats.get(&actor.token.session).is_none_or(|record|record.execution_instance!=native.execution_instance || !record.handle.same_actor(&native.handle)) {
                    return Err("The native session changed during retirement; its replacement was preserved".into());
                }
            }
            if let Some(terminal)=&actor.terminal {
                if state.terminals.get(&actor.token.session).is_none_or(|record|record.handle.execution_instance()!=terminal.instance || !record.handle.same_worker(&terminal.handle)) {
                    return Err("The terminal session changed during retirement; its replacement was preserved".into());
                }
            }
            if actor.native.is_some() {
                state.chats.remove(&actor.token.session);
                if let Some(notifications)=&state.notifications {notifications.retire(actor.token.session);}
                state.inventory_generation+=1;
            }
            if actor.terminal.is_some() {state.retire_terminal(actor.token.session);}
        }
        if !census(host,&self.path)?.is_empty() {
            return Err("Another session appeared during cleanup; refresh the remaining worktree roster".into());
        }
        Ok(Reply::SessionsStopped {path:self.path})
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn token(session: TerminalId) -> BusyAgent {
        BusyAgent { session, instance: uuid::Uuid::new_v4(), close_revision: Some(3), turn_generation: Some(2), session_instance: uuid::Uuid::new_v4() }
    }
    fn host() -> SharedHost {
        std::sync::Arc::new(super::super::state::HostLock::new(super::super::state::HostState::new(crate::model::HostSessionProof {
            host_session_id: crate::model::HostSessionId::new("worktree-consent-fixture"),
            boot_id: crate::model::BootId::new("worktree-consent-boot"),
            host_incarnation: crate::model::HostIncarnation::new("worktree-consent-incarnation"),
        })))
    }
    fn starting_session(directory: PathBuf) -> chat::Session {
        chat::Session {
            id: TerminalId::new(), proof: None, terminal: None,
            owned_codex_listener: None, native_continuity: Default::default(),
            launch: chat::Launch { provider: crate::agent::Provider::Claude, session_id: None,
                recipe: crate::model::LaunchRecipe { program: "/bin/false".into(), arguments: Vec::new(), working_directory: directory, environment: BTreeMap::new(), unset_environment: Default::default() },
            },
        }
    }
    fn history_record(directory: PathBuf) -> super::super::chat::Record {
        struct History;
        impl chat::protocol::Adapter for History {
            fn initialize(&mut self)->Result<chat::protocol::Step,String> {Ok(Default::default())}
            fn command(&mut self,_:chat::Command)->Result<chat::protocol::Step,String> {Err("Fixture history is read only".into())}
            fn receive(&mut self,_:serde_json::Value)->Result<chat::protocol::Step,String> {Ok(Default::default())}
        }
        let session=starting_session(directory.clone());
        let spec=chat::protocol::ProcessSpec { execution:None,messages:None,unset_environment:Default::default(),transport:chat::protocol::TransportSpec::History,program:"/bin/false".into(),arguments:Vec::new(),working_directory:directory,environment:BTreeMap::new() };
        let mut initial=chat::protocol::Step::default();
        let store=chat::runtime::preflight(&spec,&mut initial).unwrap();
        let handle=chat::runtime::Handle::start(spec,Box::new(History),initial,store,None,|_|{}).unwrap();
        super::super::chat::Record { execution_instance:uuid::Uuid::new_v4(),console_recipe:None,paired_terminal:None,session,handle,source_process:None,subscribers:BTreeMap::new(),observations:BTreeMap::new(),completion_observed:false,returning_terminal:false }
    }
    #[test]
    fn unscoped_history_is_preserved_without_blocking_unrelated_live_census() {
        let selected=tempfile::tempdir_in("/var/tmp").unwrap();
        let record=history_record(selected.path().join("missing-history-directory"));
        let id=record.session.id; let handle=record.handle.clone(); let host=host();
        assert!(!handle.close_observation().owns_work);
        host.lock().unwrap().chats.insert(id,record);
        assert!(census(&host,selected.path()).unwrap().is_empty());
        assert!(host.lock().unwrap().chats.contains_key(&id),"read-only history must not be retired using inferred scope");
        handle.close().unwrap();
    }
    #[test]
    fn unscoped_history_never_hides_its_retained_live_console() {
        let selected=tempfile::tempdir_in("/var/tmp").unwrap();
        let mut record=history_record(selected.path().into());
        let mut recipe=record.session.launch.recipe.clone();recipe.program="/bin/sh".into();recipe.arguments=vec!["-c".into(),"exec sleep 600".into()];
        let terminal=TerminalSession { id:record.session.id,launch:recipe.clone(),owned_codex_listener:None,recovery_origin:None,predecessor:None,transcript_path:None,resume_hint:None,continuity:crate::model::TerminalContinuity::RelaunchAvailable {recipe,last_exit:None} };
        let paired=TerminalHandle::spawn(terminal.clone(),crate::terminal::TerminalConfig::default(),|_|{}).unwrap();
        record.paired_terminal=Some(paired.clone()); record.session.terminal=Some(terminal);
        let handle=record.handle.clone(); let host=host();host.lock().unwrap().chats.insert(record.session.id,record);
        assert!(census(&host,selected.path()).is_err(),"the actual retained worker remains ownership even when absent from inventory");
        assert!(!paired.is_completed());
        paired.shutdown().unwrap();handle.close().unwrap();
    }
    #[test]
    fn detached_startup_is_in_the_candidate_census_without_any_subscriber() {
        let selected = tempfile::tempdir().unwrap();
        let unrelated = tempfile::tempdir().unwrap();
        let target = starting_session(selected.path().join("nested"));
        let other = starting_session(unrelated.path().into());
        let host = host();
        {
            let mut state = host.lock().unwrap();
            state.chat_spawning.insert(target.id, target.clone());
            state.chat_spawning.insert(other.id, other);
        }
        let actors = census(&host, selected.path()).unwrap();
        assert_eq!(actors.len(), 1);
        assert_eq!(actors[0].token.session, target.id);
        assert!(actors[0].busy && actors[0].starting);
        assert!(actors[0].token.close_revision.is_none(), "an unfinished actor never grants close authority");
    }
    #[test]
    fn close_scope_rejects_repository_subdirectories_and_other_repositories() {
        let root = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        for directory in [root.path(), other.path()] {
            assert!(std::process::Command::new("git").args(["init", "--quiet"]).arg(directory).status().unwrap().success());
        }
        std::fs::create_dir(root.path().join("nested")).unwrap();
        assert_eq!(selected_worktree(root.path(), root.path()).unwrap(), root.path().canonicalize().unwrap());
        assert!(selected_worktree(root.path(), &root.path().join("nested")).is_err());
        assert!(selected_worktree(root.path(), other.path()).is_err());
        let host = host();
        assert!(matches!(close(&host, root.path(), root.path(), &[], &[]).unwrap(), Reply::SessionsClosed {..}));
    }

    #[test]
    fn consent_requires_the_exact_unique_roster_including_idle_actors() {
        let mut census = vec![token(TerminalId::new()), token(TerminalId::new())];
        census.sort();
        assert!(validate_tokens(&census[..1], &census).is_ok());
        assert!(validate_tokens(&[], &census).is_ok());
        assert!(validate_tokens(&[census[0].clone(), census[0].clone()], &census).is_err());
        assert!(validate_tokens(&[], &[census[0].clone(), census[0].clone()]).is_err());
        let mut replacement = census[0].clone();
        replacement.instance = uuid::Uuid::new_v4();
        assert!(validate_tokens(&[replacement.clone()], &census).is_err());
        replacement = census[0].clone(); replacement.close_revision = Some(4);
        assert!(validate_tokens(&[replacement], &census).is_err());
        assert!(validate_tokens(&census, &census[..1]).is_err());
    }
    #[test]
    fn directory_scope_includes_nested_and_symlinked_launches() {
        let root = tempfile::tempdir().unwrap();
        let selected = root.path().join("worktree");
        std::fs::create_dir_all(selected.join("nested")).unwrap();
        std::os::unix::fs::symlink(&selected, root.path().join("alias")).unwrap();
        assert!(inside(&selected.join("nested"), &selected).unwrap());
        assert!(inside(&root.path().join("alias/nested"), &selected).unwrap());
        assert!(!inside(&root.path().join("other"), &selected).unwrap());
        assert!(!inside(&root.path().join("worktree-sibling"), &selected).unwrap());
        let outside = root.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, selected.join("escape")).unwrap();
        assert!(!inside(&selected.join("escape"), &selected).unwrap());
        assert!(!inside(&selected.join("../outside"), &selected).unwrap());
        assert!(!inside(&selected.join("missing/../../outside"), &selected).unwrap());
        // The final component no longer exists; its existing symlink ancestor
        // still determines which worktree actually owns the old directory.
        assert!(!inside(&selected.join("escape/gone"), &selected).unwrap());
        assert!(inside(&root.path().join("alias/gone"), &selected).unwrap());
        assert!(!inside(&selected.join("missing/../escape/gone"), &selected).unwrap());
        std::os::unix::fs::symlink(outside.join("missing"), selected.join("dangling")).unwrap();
        assert!(inside(&selected.join("dangling/child"), &selected).is_err());
    }
}
