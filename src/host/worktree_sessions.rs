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
    worktree_control::{BusyAgent, Reply},
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
type Directory = (PathBuf, crate::agent_execution::DirectoryIdentity);
struct NativeCandidate {
    execution_instance: uuid::Uuid,
    session: chat::Session,
    handle: chat::runtime::Handle,
    pair_valid: bool,
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
fn candidates(host: &SharedHost) -> Result<Candidates, String> {
    let state = host.lock().unwrap_or_else(|p| p.into_inner());
    if state.shutting_down || state.has_unscoped_spawn() {
        return Err("A daemon transition has not established an attributable working directory; refresh after it settles".into());
    }
    let terminals = state.terminals.values().map(|record| (
        record.handle.execution_instance(), record.session.clone(), record.handle.clone(), record.handle.admitted_directory().map(|(path, identity)| (path.to_owned(), identity)),
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
            paired_directory: record.paired_terminal.as_ref().map(|handle| handle.admitted_directory().map(|(path, identity)| (path.to_owned(), identity))),
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
        let (directory, identity) = directory.ok_or_else(|| format!("Terminal {} has no authoritative admitted physical directory; close it explicitly before worktree removal", session.id))?;
        // Operational/launch aliases may expand removal protection, but cannot
        // newly authorize closing an actor admitted in another physical scope.
        if !admitted_scope(&directory, identity, path)? { continue; }
        let (revision, busy) = handle.close_observation()?;
        let token = BusyAgent { session: session.id, instance, close_revision: Some(revision), turn_generation: None };
        actors.insert(session.id, Actor { token, busy, terminal: Some(Terminal { instance, session, handle, revision }), native: None, starting: false });
    }
    for NativeCandidate { execution_instance, session, handle, pair_valid, paired_directory } in candidates.natives {
        let observation = handle.close_observation();
        let identity = observation.directory_identity.ok_or_else(|| format!("Native session {} has no authoritative physical scope; reconnect or explicitly close it before worktree removal", session.id))?;
        let native_inside = admitted_scope(&observation.working_directory, identity, path)?;
        if let Some(paired_directory) = paired_directory {
            let (directory, identity) = paired_directory.ok_or("A paired terminal has no authoritative physical scope; its actors were preserved")?;
            let terminal_inside = admitted_scope(&directory, identity, path)?;
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
            token: BusyAgent { session: session.id, instance, close_revision: None, turn_generation: None },
            busy: false, terminal: None, native: None, starting: false,
        });
        if let Some(terminal) = &actor.terminal {
            // The console is one logical actor, but replacing it or admitting
            // input must invalidate the native actor's composite consent too.
            let mut digest = Sha256::new();
            digest.update(instance.as_bytes()); digest.update(terminal.instance.as_bytes()); digest.update(terminal.revision.to_le_bytes());
            let hash = digest.finalize();
            instance = uuid::Uuid::from_bytes(hash[..16].try_into().expect("SHA-256 prefix"));
        }
        actor.token = BusyAgent { session: session.id, instance, close_revision: Some(observation.close_revision), turn_generation: Some(observation.turn_generation) };
        actor.busy |= observation.owns_work && (!observation.ready || matches!(observation.activity, ActivityState::Working | ActivityState::Waiting | ActivityState::Unknown));
        actor.native = Some(Native { execution_instance, session, handle, observation });
    }
    for (id, instance, directory) in candidates.starting {
        if !inside(&directory, path)? { continue; }
        let actor = actors.entry(id).or_insert_with(|| Actor {
            token: BusyAgent { session: id, instance, close_revision: None, turn_generation: None },
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

#[cfg(test)]
mod tests {
    use super::*;
    fn token(session: TerminalId) -> BusyAgent {
        BusyAgent { session, instance: uuid::Uuid::new_v4(), close_revision: Some(3), turn_generation: Some(2) }
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
