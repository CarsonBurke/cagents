//! Project removal holds control admission, never a database transaction,
//! across source-bound provider teardown. Both identity components are checked
//! before effects and again in the final atomic edit.
use super::state::{ClientId, SharedHost};
use crate::{agent_execution::HostedSession, project_recovery::{ProjectInstance, SessionClose}, terminal_ipc::{ClientRequest, ProtocolError, ProtocolErrorCode, ServerMessage, ServerResponse}, workspace_model::{ModelEdit, views::Scope}};
use std::{collections::HashSet, time::{Duration, Instant}};

const MAX_SESSIONS: usize = 4096;
fn error(message: impl Into<String>) -> ProtocolError { ProtocolError::new(ProtocolErrorCode::InvalidRequest, message, false) }
enum Owner { Chat(crate::chat::runtime::Handle), Terminal(crate::terminal::TerminalHandle) }

pub(super) fn inventory(host: &SharedHost, client: ClientId, project: &ProjectInstance) -> Result<Vec<SessionClose>, ProtocolError> {
    let registry = super::agent_execution::registry(host).map_err(error)?;
    registry.lock().unwrap_or_else(|p| p.into_inner()).require_project_instance(project).map_err(error)?;
    let state = host.lock().unwrap_or_else(|p| p.into_inner());
    if !state.chat_spawning.is_empty() { return Err(error("A native session is still starting; wait for it before removing the project")); }
    let mut sessions = state.chats.values().filter(|record| record.handle.owns_work() || record.returning_terminal).map(|record| {
        let mut session = record.session.clone();
        session.launch.session_id = record.handle.session_id();
        session.native_continuity = record.handle.native_continuity();
        SessionClose { source: HostedSession::Chat(session), execution_instance: Some(record.execution_instance) }
    }).collect::<Vec<_>>();
    sessions.extend(state.list(client).into_iter().map(|terminal| SessionClose { source: HostedSession::Terminal(terminal.session), execution_instance: Some(terminal.execution_instance) }));
    if sessions.len() > MAX_SESSIONS { return Err(error("Too many hosted sessions to inventory safely; close some panes and retry")); }
    Ok(sessions)
}

pub(super) fn remove(host: &SharedHost, client: ClientId, project: ProjectInstance, sessions: Vec<SessionClose>) -> Result<(ServerResponse, Vec<ServerMessage>, bool), ProtocolError> {
    if sessions.len() > MAX_SESSIONS { return Err(error("Too many project sessions to close at once; close some panes and retry")); }
    let registry = super::agent_execution::registry(host).map_err(error)?;
    let model = {
        let registry = registry.lock().unwrap_or_else(|p| p.into_inner());
        registry.require_project_instance(&project).map_err(error)?;
        registry.model().map_err(error)?
    };
    let mut foreign = HashSet::new();
    let mut owned = HashSet::new();
    let mut roots = model.project(&project.repository).into_iter().flat_map(|project| project.primary_checkout.iter().chain(project.checkout.iter()).cloned().chain((project.kind == crate::model::ProjectKind::Folder).then(|| project.repository.clone()))).collect::<Vec<_>>();
    for view in &model.views {
        let repository = match &view.scope {
            Scope::Worktree { repository, .. } => Some(repository),
            Scope::Channel(channel) => model.channel(*channel).map(|channel| &channel.repository),
        };
        if repository == Some(&project.repository) {
            if let Scope::Worktree { path, .. } = &view.scope { roots.push(path.clone()); }
            for pane in &view.panes {
                match &pane.kind {
                    crate::model::CardKind::Terminal(session) => { owned.insert(session.id); },
                    crate::model::CardKind::Chat(session) => { owned.insert(session.id); if let Some(terminal) = &session.terminal { owned.insert(terminal.id); } },
                    _ => {},
                }
            }
            continue;
        }
        for pane in &view.panes {
            match &pane.kind {
                crate::model::CardKind::Terminal(session) => { foreign.insert(session.id); },
                crate::model::CardKind::Chat(session) => { foreign.insert(session.id); if let Some(terminal) = &session.terminal { foreign.insert(terminal.id); } },
                _ => {},
            }
        }
    }
    for agent in &model.agents {
        let repository = model.agent_locations.iter().find(|location| location.agent == agent.id).map(|location| &location.repository).unwrap_or(&agent.spec.target.repository);
        if *repository == project.repository {
            if let Some(source) = &agent.session { owned.insert(source.id()); }
            if let Some(console) = &agent.console { owned.insert(console.id); }
        } else {
            if let Some(source) = &agent.session { foreign.insert(source.id()); }
            if let Some(console) = &agent.console { foreign.insert(console.id); }
        }
    }
    // A start admitted before this request must not escape its confirmation.
    let selected_ids = sessions.iter().map(|session| (session.source.id(), matches!(session.source, HostedSession::Chat(_)))).collect::<HashSet<_>>();
    for current in inventory(host, client, &project)? {
        let id = current.source.id();
        let directory = match &current.source { HostedSession::Chat(source) => &source.launch.recipe.working_directory, HostedSession::Terminal(source) => &source.launch.working_directory };
        if !foreign.contains(&id) && (owned.contains(&id) || roots.iter().any(|root| directory.starts_with(root)))
            && !selected_ids.contains(&(id, matches!(current.source, HostedSession::Chat(_)))) {
            return Err(error("A new session appeared in this project after its removal inventory. Refresh Projects and retry; no sessions were closed."));
        }
    }
    // Validate the complete roster before closing even its first provider.
    let owners = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        let mut seen = HashSet::new();
        sessions.iter().map(|selected| {
            let id = selected.source.id();
            let kind = matches!(selected.source, HostedSession::Chat(_));
            if !seen.insert((id, kind)) { return Err(error("Project removal contains a repeated session source")); }
            if foreign.contains(&id) { return Err(error(format!("Session {id} is retained by another project; no project sessions were closed"))); }
            match &selected.source {
                HostedSession::Chat(source) => {
                    let record = state.chats.get(&id).ok_or_else(|| error("A selected native session changed; refresh Projects before removing it"))?;
                    let mut current = record.session.clone();
                    current.launch.session_id = record.handle.session_id();
                    if selected.execution_instance != Some(record.execution_instance) || !HostedSession::Chat(current.clone()).same_process(&selected.source) { return Err(error("A selected native session changed; no project sessions were closed")); }
                    Ok((Owner::Chat(record.handle.clone()), HostedSession::Chat(current)))
                },
                HostedSession::Terminal(source) => {
                    let current = state.descriptor(client, id)?;
                    if selected.execution_instance.is_none_or(|instance| instance.is_nil() || current.execution_instance != instance) || current.session != *source { return Err(error("A selected terminal worker changed; no project sessions were closed")); }
                    let record = state.terminals.get(&id).ok_or_else(|| error("A selected terminal worker is no longer hosted"))?;
                    Ok((Owner::Terminal(record.handle.clone()), HostedSession::Terminal(current.session)))
                },
            }
        }).collect::<Result<Vec<_>,ProtocolError>>()?
    };
    for (selected, (_, current)) in sessions.iter().zip(&owners) {
        if matches!(selected.source, HostedSession::Chat(_)) && !registry.lock().unwrap_or_else(|p| p.into_inner()).authenticated_session_alias(&selected.source, current).map_err(error)? {
            return Err(error("A selected native thread or source changed; no project sessions were closed"));
        }
    }
    let deadline = Instant::now() + Duration::from_secs(60);
    for (selected, (owner, _)) in sessions.into_iter().zip(owners) {
        if Instant::now() >= deadline { return Err(error("Project teardown reached its time limit. The project remains open; retry to close the remaining sessions.")); }
        registry.lock().unwrap_or_else(|p| p.into_inner()).require_project_instance(&project).map_err(error)?;
        let id = selected.source.id();
        match (selected.source, owner) {
            (HostedSession::Chat(source), Owner::Chat(owner)) => {
                let state = host.lock().unwrap_or_else(|p| p.into_inner());
                let unchanged = state.chats.get(&id).is_some_and(|record| record.handle.same_actor(&owner));
                if !unchanged { if owner.is_completed() && !state.chats.contains_key(&id) { continue; } return Err(error("A selected native actor changed; its replacement was preserved")); }
                drop(state);
                super::chat::process(host, client, ClientRequest::ChatAttach { session: source })?;
                super::chat::process(host, client, ClientRequest::ChatClose { chat_id: id })?;
            },
            (HostedSession::Terminal(source), Owner::Terminal(owner)) => {
                let state = host.lock().unwrap_or_else(|p| p.into_inner());
                if !state.terminals.get(&id).is_some_and(|record| record.handle.same_worker(&owner)) {
                    if owner.is_completed() && !state.terminals.contains_key(&id) { continue; }
                    return Err(error("A selected terminal worker changed; its replacement was preserved"));
                }
                drop(state);
                let request = ClientRequest::CloseTerminalSourceChecked { source, execution_instance: selected.execution_instance.ok_or_else(|| error("Missing terminal worker identity"))?, allow_active: true };
                let (response, _, _) = super::client::process_admitted(host, client, request)?;
                if !matches!(response, ServerResponse::TerminalCloseChecked { outcome: crate::terminal_ipc::TerminalCloseResult::Closed, .. }) { return Err(error("The exact terminal worker did not confirm teardown; the project remains open")); }
            },
            _ => unreachable!("preflight retained the selected source kind"),
        }
    }
    super::client::process_admitted(host, client, ClientRequest::EditWorkspaceModel { edit: ModelEdit::CloseProjectInstance { project } })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{host::{model::Registry, state::{HostLock, HostState}}, model::{BootId, HostIncarnation, HostSessionId, HostSessionProof, LaunchRecipe, ProjectKind, TerminalContinuity, TerminalSession}, terminal::TerminalHandle};
    use std::{path::PathBuf, sync::Arc};
    struct Fixture { _directory: tempfile::TempDir, root: PathBuf, host: SharedHost, expected: ProjectInstance, handles: Vec<TerminalHandle> }
    impl Drop for Fixture { fn drop(&mut self) { for handle in &self.handles { let _ = handle.shutdown(); } } }
    impl Fixture {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path().join("project"); std::fs::create_dir(&root).unwrap();
            let mut registry = Registry::at(&directory.path().join("registry.sqlite3")).unwrap();
            registry.apply(&ModelEdit::OpenProject { repository: root.clone(), checkout: None, primary_checkout: None, name: "Project".into(), kind: ProjectKind::Folder }).unwrap();
            let model = registry.model().unwrap();
            let expected = ProjectInstance { registry: model.identity, repository: root.clone(), ordinal: model.projects[0].ordinal };
            let proof = HostSessionProof { host_session_id: HostSessionId::new("guarded-project-close"), boot_id: BootId::new("boot"), host_incarnation: HostIncarnation::new("incarnation") };
            let mut state = HostState::new(proof); state.install_model(registry); state.register_client(1);
            Self { _directory: directory, root, host: Arc::new(HostLock::new(state)), expected, handles: Vec::new() }
        }
        fn spawn(&mut self, id: crate::model::TerminalId) -> SessionClose {
            let mut recipe = LaunchRecipe::shell(self.root.clone()).unwrap(); recipe.program = "/bin/cat".into(); recipe.arguments.clear();
            let session = TerminalSession { id, launch: recipe.clone(), owned_codex_listener: None, recovery_origin: None, predecessor: None, transcript_path: None, resume_hint: None, continuity: TerminalContinuity::RelaunchAvailable { recipe, last_exit: None } };
            let handle = TerminalHandle::spawn(session.clone(), Default::default(), |_| {}).unwrap();
            let descriptor = { let mut state = self.host.lock().unwrap(); state.reserve_spawn(id).unwrap(); state.finish_spawn(1, session, handle.clone(), true).unwrap() };
            self.handles.push(handle);
            SessionClose { source: HostedSession::Terminal(descriptor.session), execution_instance: Some(descriptor.execution_instance) }
        }
        fn request(&self, expected: ProjectInstance, sessions: Vec<SessionClose>) -> Result<(ServerResponse, Vec<ServerMessage>, bool),ProtocolError> {
            super::super::client::process_request(&self.host, 1, ClientRequest::CloseProjectGuarded { project: expected, sessions })
        }
    }
    #[test]
    fn stale_project_and_registry_incarnations_refuse_before_teardown() {
        let mut fixture = Fixture::new(); let selected = fixture.spawn(crate::model::TerminalId::new());
        {
            let mut state = fixture.host.lock().unwrap();
            state.edit_model(1, ModelEdit::CloseProject { repository: fixture.root.clone() }).unwrap();
            state.edit_model(1, ModelEdit::OpenProject { repository: fixture.root.clone(), checkout: None, primary_checkout: None, name: "Replacement".into(), kind: ProjectKind::Folder }).unwrap();
        }
        assert!(fixture.request(fixture.expected.clone(), vec![selected.clone()]).is_err());
        assert!(!fixture.handles[0].is_completed(), "reopened path must preserve its live process before any close");
        let replacement_path = fixture._directory.path().join("recreated.sqlite3");
        let mut recreated = Registry::at(&replacement_path).unwrap();
        recreated.apply(&ModelEdit::OpenProject { repository: fixture.root.clone(), checkout: None, primary_checkout: None, name: "Recreated".into(), kind: ProjectKind::Folder }).unwrap();
        let model = recreated.model().unwrap(); assert_eq!(model.projects[0].ordinal, fixture.expected.ordinal);
        fixture.host.lock().unwrap().install_model(recreated);
        assert!(fixture.request(fixture.expected.clone(), vec![selected.clone()]).is_err());
        assert!(!fixture.handles[0].is_completed(), "same rank in a recreated registry cannot authorize teardown");
        let valid = ProjectInstance { registry: model.identity, repository: fixture.root.clone(), ordinal: model.projects[0].ordinal };
        let (response,_,_) = fixture.request(valid, vec![selected]).unwrap();
        let ServerResponse::WorkspaceModel { model } = response else { panic!() };
        assert!(model.project(&fixture.root).is_none());
        assert!(fixture.handles[0].is_completed());
    }
    #[test]
    fn changed_worker_or_new_session_refuses_the_entire_roster_before_first_close() {
        let mut fixture = Fixture::new(); let first = fixture.spawn(crate::model::TerminalId::new());
        let second = fixture.spawn(crate::model::TerminalId::new());
        assert!(fixture.request(fixture.expected.clone(), vec![first.clone()]).is_err());
        assert!(fixture.handles.iter().all(|handle| !handle.is_completed()), "a missing new session must not partially close the earlier roster");
        fixture.handles[1].shutdown().unwrap(); fixture.host.lock().unwrap().retire_terminal(second.source.id());
        let successor = fixture.spawn(second.source.id());
        assert_eq!(second.source, successor.source, "full recipe and daemon proof do not identify the worker");
        assert_ne!(second.execution_instance, successor.execution_instance);
        assert!(fixture.request(fixture.expected.clone(), vec![first.clone(), second]).is_err());
        assert!(!fixture.handles[0].is_completed()); assert!(!fixture.handles[2].is_completed());
        fixture.request(fixture.expected.clone(), vec![first, successor]).unwrap();
        assert!(fixture.handles.iter().all(TerminalHandle::is_completed));
    }
}
