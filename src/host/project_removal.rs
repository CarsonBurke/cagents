//! Project removal reserves only the selected logical project. Slow source-
//! bound teardown shares control admission with unrelated input and reads;
//! short exclusive admission installs the reservation and commits the edit.
use super::state::{ClientId, SharedHost};
use crate::{agent_execution::HostedSession, project_recovery::{ProjectInstance, SessionClose}, terminal_ipc::{ClientRequest, ProtocolError, ProtocolErrorCode, ServerMessage, ServerResponse}, workspace_model::{ModelEdit, views::Scope}};
use std::{collections::HashSet, time::{Duration, Instant}};

mod reservation;
pub(super) use reservation::{Reservations, admit, ensure_available, ensure_agent_available, reserve};
use reservation::Reservation;

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

pub(super) fn remove(host: &SharedHost, client: ClientId, project: ProjectInstance, sessions: Vec<SessionClose>, _reservation: &Reservation) -> Result<(ServerResponse, Vec<ServerMessage>, bool), ProtocolError> {
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
    let gate = host.lock().unwrap_or_else(|p| p.into_inner()).control_gate.clone();
    let deadline = Instant::now() + Duration::from_secs(60);
    for (selected, (owner, _)) in sessions.into_iter().zip(owners) {
        let _shared = gate.read().unwrap_or_else(|p| p.into_inner());
        super::client::ensure_control_open(host)?;
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
    let _commit = gate.write().unwrap_or_else(|p| p.into_inner());
    super::client::ensure_control_open(host)?;
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
        fn spawn(&mut self, id: crate::model::TerminalId) -> SessionClose { let root = self.root.clone(); self.spawn_at(id,&root) }
        fn spawn_at(&mut self, id: crate::model::TerminalId, directory: &std::path::Path) -> SessionClose {
            let mut recipe = LaunchRecipe::shell(directory.to_owned()).unwrap(); recipe.program = "/bin/cat".into(); recipe.arguments.clear();
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
    fn slow_project_teardown_keeps_unrelated_input_responsive_and_releases_reservation() {
        use crate::{agent::Provider, chat::{self, Command, protocol::{Adapter, ProcessSpec, Step}, runtime::Handle}};
        use std::{collections::BTreeMap, thread};
        struct Delayed { entered: crossbeam_channel::Sender<()>, release: crossbeam_channel::Receiver<()> }
        impl Adapter for Delayed {
            fn initialize(&mut self) -> Result<Step,String> { Ok(Step::default()) }
            fn command(&mut self,_:Command) -> Result<Step,String> { self.entered.send(()).unwrap(); self.release.recv_timeout(Duration::from_secs(5)).map_err(|error| error.to_string())?; Ok(Step::default()) }
            fn receive(&mut self,_:serde_json::Value) -> Result<Step,String> { Ok(Step::default()) }
        }
        struct Release(crossbeam_channel::Sender<()>);
        impl Drop for Release { fn drop(&mut self) { let _ = self.0.try_send(()); } }
        let mut fixture = Fixture::new();
        let unrelated_dir = fixture._directory.path().join("unrelated"); std::fs::create_dir(&unrelated_dir).unwrap();
        let unrelated = fixture.spawn_at(crate::model::TerminalId::new(), &unrelated_dir);
        let mut recipe = LaunchRecipe::shell(fixture.root.clone()).unwrap(); recipe.program = "/bin/cat".into(); recipe.arguments.clear();
        let spec = ProcessSpec::from(&recipe); let mut initial = Step::default(); let store = chat::runtime::preflight(&spec, &mut initial).unwrap();
        let (entered, pending) = crossbeam_channel::bounded(1); let (release, released) = crossbeam_channel::bounded(1); let release = Release(release);
        let handle = Handle::start(spec,Box::new(Delayed { entered, release: released }),initial,store,None,|_|{}).unwrap();
        let id = crate::model::TerminalId::new(); let instance = uuid::Uuid::new_v4();
        let session = {
            let mut state = fixture.host.lock().unwrap();
            let session = chat::Session { id, launch: chat::Launch { provider: Provider::Claude, recipe, session_id: None }, proof: Some(state.proof().clone()), terminal: None, native_continuity: Default::default(), owned_codex_listener: None };
            state.chats.insert(id,super::super::chat::Record { execution_instance: instance, console_recipe: None, paired_terminal: None, session: session.clone(), handle: handle.clone(), source_process: None, subscribers: BTreeMap::from([(1,false)]), observations: BTreeMap::new(), completion_observed: false, returning_terminal: false });
            session
        };
        let actor = handle.clone(); let command = thread::spawn(move || actor.command(Command::Interrupt));
        pending.recv_timeout(Duration::from_secs(2)).unwrap();
        let host = fixture.host.clone(); let expected = fixture.expected.clone();
        let (closed, closing) = crossbeam_channel::bounded(1);
        let close = thread::spawn(move || { let result = super::super::client::process_request(&host,1,ClientRequest::CloseProjectGuarded { project: expected, sessions: vec![SessionClose { source: HostedSession::Chat(session), execution_instance: Some(instance) }] }); closed.send(result).unwrap(); });
        let deadline = Instant::now() + Duration::from_secs(2);
        while fixture.host.lock().unwrap().project_removals.lock().unwrap().is_empty() { assert!(Instant::now() < deadline,"scoped removal was not admitted"); thread::yield_now(); }
        assert!(closing.recv_timeout(Duration::from_millis(50)).is_err(),"provider remains blocked until explicitly released");
        let host = fixture.host.clone(); let unrelated_id = unrelated.source.id();
        let (answered, answering) = crossbeam_channel::bounded(1);
        let input = thread::spawn(move || { let listed = super::super::client::process_request(&host,1,ClientRequest::List); let input = super::super::client::process_request(&host,1,ClientRequest::Input { terminal_id: unrelated_id, bytes: b"hello\n".to_vec() }); answered.send((listed,input)).unwrap(); });
        let (listed,input_result) = answering.recv_timeout(Duration::from_secs(1)).expect("unrelated input/list must complete before provider release");
        assert!(listed.is_ok()); assert!(input_result.is_ok()); input.join().unwrap();
        let blocked = super::super::client::process_request(&fixture.host,1,ClientRequest::EditWorkspaceModel { edit: ModelEdit::ReopenProject { repository: fixture.root.clone(), name: "Retry".into(), kind: ProjectKind::Folder, checkout: None, primary_checkout: None } }).unwrap_err();
        assert!(blocked.message.contains("being removed"));
        let transfer = ClientRequest::EditWorkspaceModel { edit: ModelEdit::ChangeView { scope: Scope::Worktree { repository: unrelated_dir.clone(), path: unrelated_dir }, expected_revision:0, operation:uuid::Uuid::new_v4(), change:crate::workspace_model::views::Change::Transfer { id: crate::model::CardId::new(), destination: Scope::Worktree { repository: fixture.root.clone(), path: fixture.root.clone() }, before:None } } };
        assert!(super::super::client::process_request(&fixture.host,1,transfer).unwrap_err().message.contains("being removed"));
        assert!(ensure_available(&fixture.host,None,Some(&fixture.root),None).is_err());
        release.0.send(()).unwrap(); let _ = command.join().unwrap();
        assert!(closing.recv_timeout(Duration::from_secs(5)).unwrap().is_ok()); close.join().unwrap();
        assert!(!fixture.handles[0].is_completed()); assert!(handle.is_completed());
        assert!(fixture.host.lock().unwrap().project_removals.lock().unwrap().is_empty());
        assert!(ensure_available(&fixture.host,None,Some(&fixture.root),None).is_ok());
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
        assert!(fixture.host.lock().unwrap().project_removals.lock().unwrap().is_empty(), "failed preflight releases its scoped reservation");
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
