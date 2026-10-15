//! Project removal reserves only the selected logical project. Slow source-
//! bound teardown shares control admission with unrelated input and reads;
//! short exclusive admission installs the reservation and commits the edit.
use super::state::{ClientId, SharedHost};
use crate::{
    agent_execution::HostedSession,
    project_recovery::{ProjectInstance, ProviderIncarnation, SessionClose},
    terminal_ipc::{
        ClientRequest, ProtocolError, ProtocolErrorCode, ServerMessage, ServerResponse,
    },
    workspace_model::{ModelEdit, views::Scope},
};
use std::{
    collections::HashSet,
    time::{Duration, Instant},
};

mod reservation;
mod census;
use reservation::Reservation;
pub(super) use reservation::{
    Reservations, admit, ensure_agent_available, ensure_available, reserve,
};

const MAX_SESSIONS: usize = 4096;
fn error(message: impl Into<String>) -> ProtocolError {
    ProtocolError::new(ProtocolErrorCode::InvalidRequest, message, false)
}
enum Owner {
    Chat(crate::chat::runtime::Handle),
    Terminal(crate::terminal::TerminalHandle),
}

pub(super) fn inventory(
    host: &SharedHost,
    client: ClientId,
    project: &ProjectInstance,
) -> Result<Vec<SessionClose>, ProtocolError> {
    let registry = super::agent_execution::registry(host).map_err(error)?;
    registry
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .require_project_instance(project)
        .map_err(error)?;
    let (chats, mut sessions) = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        if !state.chat_spawning.is_empty() {
            return Err(error(
                "A native session is still starting; wait for it before removing the project",
            ));
        }
        let chats = state
            .chats
            .values()
            .filter(|record| record.handle.owns_work() || record.returning_terminal)
            .map(|record| {
                let mut session = record.session.clone();
                session.launch.session_id = record.handle.session_id();
                session.native_continuity = record.handle.native_continuity();
                (session, record.execution_instance, record.handle.clone())
            })
            .collect::<Vec<_>>();
        let sessions = state
            .list(client)
            .into_iter()
            .map(|terminal| SessionClose {
                source: HostedSession::Terminal(terminal.session),
                execution_instance: Some(terminal.execution_instance),
                provider: None,
            })
            .collect::<Vec<_>>();
        (chats, sessions)
    };
    for (session, instance, actor) in chats {
        // Authenticated retained state refresh may perform IPC. Never do it
        // under HostState; final teardown refreshes/rechecks under its lifecycle gate.
        actor.refresh_retained_state().map_err(error)?;
        sessions.push(SessionClose {
            source: HostedSession::Chat(session),
            execution_instance: Some(instance),
            provider: actor
                .provider_process()
                .map(ProviderIncarnation::from_process),
        });
    }
    if sessions.len() > MAX_SESSIONS {
        return Err(error(
            "Too many hosted sessions to inventory safely; close some panes and retry",
        ));
    }
    Ok(sessions)
}

pub(super) fn remove(
    host: &SharedHost,
    client: ClientId,
    project: ProjectInstance,
    sessions: Vec<SessionClose>,
    reservation: &Reservation,
) -> Result<(ServerResponse, Vec<ServerMessage>, bool), ProtocolError> {
    if sessions.len() > MAX_SESSIONS {
        return Err(error(
            "Too many project sessions to close at once; close some panes and retry",
        ));
    }
    let registry = super::agent_execution::registry(host).map_err(error)?;
    registry
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .require_project_instance(&project)
        .map_err(error)?;
    // Filesystem/holder IPC is deliberately outside HostState and exclusive
    // control admission. Unknown retained ownership fails the whole roster.
    census::require_no_unhosted_owners(host, reservation)?;
    // A start admitted before this request must not escape its confirmation.
    let selected_ids = sessions
        .iter()
        .map(|session| {
            (
                session.source.id(),
                matches!(session.source, HostedSession::Chat(_)),
            )
        })
        .collect::<HashSet<_>>();
    for current in inventory(host, client, &project)? {
        let id = current.source.id();
        if reservation.qualifies(&current.source)
            && !selected_ids.contains(&(id, matches!(current.source, HostedSession::Chat(_))))
        {
            return Err(error(
                "A new session appeared in this project after its removal inventory. Refresh Projects and retry; no sessions were closed.",
            ));
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
            if !reservation.qualifies(&selected.source) { return Err(error(format!("Session {id} is not verified as owned by this project; no project sessions were closed"))); }
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
    for (selected, (owner, _)) in sessions.iter().zip(&owners) {
        if let Owner::Chat(actor) = owner {
            actor.refresh_retained_state().map_err(error)?;
            if selected.provider.is_none()
                || actor
                    .provider_process()
                    .map(ProviderIncarnation::from_process)
                    != selected.provider
            {
                return Err(error(
                    "A selected provider process changed or cannot be proved; no project sessions were closed. Refresh Projects and retry.",
                ));
            }
        }
    }
    for (selected, (_, current)) in sessions.iter().zip(&owners) {
        if matches!(selected.source, HostedSession::Chat(_))
            && !registry
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .authenticated_session_alias(&selected.source, current)
                .map_err(error)?
        {
            return Err(error(
                "A selected native thread or source changed; no project sessions were closed",
            ));
        }
    }
    let gate = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .control_gate
        .clone();
    let deadline = Instant::now() + Duration::from_secs(60);
    for (selected, (owner, _)) in sessions.into_iter().zip(owners) {
        let _shared = gate.read().unwrap_or_else(|p| p.into_inner());
        super::client::ensure_control_open(host)?;
        if Instant::now() >= deadline {
            return Err(error(
                "Project teardown reached its time limit. The project remains open; retry to close the remaining sessions.",
            ));
        }
        registry
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .require_project_instance(&project)
            .map_err(error)?;
        let id = selected.source.id();
        match (selected.source, owner) {
            (HostedSession::Chat(source), Owner::Chat(owner)) => {
                let state = host.lock().unwrap_or_else(|p| p.into_inner());
                let unchanged = state
                    .chats
                    .get(&id)
                    .is_some_and(|record| record.handle.same_actor(&owner));
                if !unchanged {
                    if owner.is_completed() && !state.chats.contains_key(&id) {
                        continue;
                    }
                    return Err(error(
                        "A selected native actor changed; its replacement was preserved",
                    ));
                }
                drop(state);
                super::chat::close_project_session(
                    host,
                    client,
                    &source,
                    selected
                        .execution_instance
                        .ok_or_else(|| error("Missing native actor identity"))?,
                    &owner,
                    selected
                        .provider
                        .as_ref()
                        .ok_or_else(|| error("Missing provider process identity"))?,
                    deadline.min(Instant::now() + Duration::from_secs(10)),
                )?;
            }
            (HostedSession::Terminal(source), Owner::Terminal(owner)) => {
                let state = host.lock().unwrap_or_else(|p| p.into_inner());
                if !state
                    .terminals
                    .get(&id)
                    .is_some_and(|record| record.handle.same_worker(&owner))
                {
                    if owner.is_completed() && !state.terminals.contains_key(&id) {
                        continue;
                    }
                    return Err(error(
                        "A selected terminal worker changed; its replacement was preserved",
                    ));
                }
                drop(state);
                let request = ClientRequest::CloseTerminalSourceChecked {
                    source,
                    execution_instance: selected
                        .execution_instance
                        .ok_or_else(|| error("Missing terminal worker identity"))?,
                    allow_active: true,
                };
                let (response, _, _) = super::client::process_admitted(host, client, request)?;
                if !matches!(
                    response,
                    ServerResponse::TerminalCloseChecked {
                        outcome: crate::terminal_ipc::TerminalCloseResult::Closed,
                        ..
                    }
                ) {
                    return Err(error(
                        "The exact terminal worker did not confirm teardown; the project remains open",
                    ));
                }
            }
            _ => unreachable!("preflight retained the selected source kind"),
        }
    }
    let _commit = gate.write().unwrap_or_else(|p| p.into_inner());
    super::client::ensure_control_open(host)?;
    super::client::process_admitted(
        host,
        client,
        ClientRequest::EditWorkspaceModel {
            edit: ModelEdit::CloseProjectInstance { project },
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        host::{
            model::Registry,
            state::{HostLock, HostState},
        },
        model::{
            BootId, HostIncarnation, HostSessionId, HostSessionProof, LaunchRecipe, ProjectKind,
            TerminalContinuity, TerminalSession,
        },
        terminal::TerminalHandle,
    };
    use std::{path::PathBuf, sync::Arc};
    struct Fixture {
        _directory: tempfile::TempDir,
        root: PathBuf,
        host: SharedHost,
        expected: ProjectInstance,
        handles: Vec<TerminalHandle>,
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            for handle in &self.handles {
                let _ = handle.shutdown();
            }
        }
    }
    impl Fixture {
        fn new() -> Self {
            Self::with_proof(HostSessionProof {
                host_session_id: HostSessionId::new("guarded-project-close"),
                boot_id: BootId::new("boot"),
                host_incarnation: HostIncarnation::new("incarnation"),
            })
        }
        fn with_proof(proof: HostSessionProof) -> Self {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path().join("project");
            std::fs::create_dir(&root).unwrap();
            let mut registry = Registry::at(&directory.path().join("registry.sqlite3")).unwrap();
            registry
                .apply(&ModelEdit::OpenProject {
                    repository: root.clone(),
                    checkout: None,
                    primary_checkout: None,
                    name: "Project".into(),
                    kind: ProjectKind::Folder,
                })
                .unwrap();
            let model = registry.model().unwrap();
            let expected = ProjectInstance {
                registry: model.identity,
                repository: root.clone(),
                ordinal: model.projects[0].ordinal,
            };
            let mut state = HostState::new(proof);
            state.install_model(registry);
            state.register_client(1);
            Self {
                _directory: directory,
                root,
                host: Arc::new(HostLock::new(state)),
                expected,
                handles: Vec::new(),
            }
        }
        fn spawn(&mut self, id: crate::model::TerminalId) -> SessionClose {
            let root = self.root.clone();
            self.spawn_at(id, &root)
        }
        fn spawn_at(
            &mut self,
            id: crate::model::TerminalId,
            directory: &std::path::Path,
        ) -> SessionClose {
            let mut recipe = LaunchRecipe::shell(directory.to_owned()).unwrap();
            recipe.program = "/bin/cat".into();
            recipe.arguments.clear();
            let session = TerminalSession {
                id,
                launch: recipe.clone(),
                owned_codex_listener: None,
                recovery_origin: None,
                predecessor: None,
                transcript_path: None,
                resume_hint: None,
                continuity: TerminalContinuity::RelaunchAvailable {
                    recipe,
                    last_exit: None,
                },
            };
            let handle =
                TerminalHandle::spawn(session.clone(), Default::default(), |_| {}).unwrap();
            let descriptor = {
                let mut state = self.host.lock().unwrap();
                state.reserve_spawn(id).unwrap();
                state
                    .finish_spawn(1, session, handle.clone(), true)
                    .unwrap()
            };
            self.handles.push(handle);
            SessionClose {
                source: HostedSession::Terminal(descriptor.session),
                execution_instance: Some(descriptor.execution_instance),
                provider: None,
            }
        }
        fn request(
            &self,
            expected: ProjectInstance,
            sessions: Vec<SessionClose>,
        ) -> Result<(ServerResponse, Vec<ServerMessage>, bool), ProtocolError> {
            super::super::client::process_request(
                &self.host,
                1,
                ClientRequest::CloseProjectGuarded {
                    project: expected,
                    sessions,
                },
            )
        }
    }
    #[test]
    fn slow_project_teardown_keeps_unrelated_input_responsive_and_releases_reservation() {
        use crate::{
            agent::Provider,
            chat::{
                self, Command,
                protocol::{Adapter, ProcessSpec, Step},
                runtime::Handle,
            },
        };
        use std::{collections::BTreeMap, thread};
        struct Delayed {
            entered: crossbeam_channel::Sender<()>,
            release: crossbeam_channel::Receiver<()>,
        }
        impl Adapter for Delayed {
            fn initialize(&mut self) -> Result<Step, String> {
                Ok(Step::default())
            }
            fn command(&mut self, _: Command) -> Result<Step, String> {
                self.entered.send(()).unwrap();
                self.release
                    .recv_timeout(Duration::from_secs(5))
                    .map_err(|error| error.to_string())?;
                Ok(Step::default())
            }
            fn receive(&mut self, _: serde_json::Value) -> Result<Step, String> {
                Ok(Step::default())
            }
        }
        struct Release(crossbeam_channel::Sender<()>);
        impl Drop for Release {
            fn drop(&mut self) {
                let _ = self.0.try_send(());
            }
        }
        let mut fixture = Fixture::new();
        let unrelated_dir = fixture._directory.path().join("unrelated");
        std::fs::create_dir(&unrelated_dir).unwrap();
        let unrelated = fixture.spawn_at(crate::model::TerminalId::new(), &unrelated_dir);
        let mut recipe = LaunchRecipe::shell(fixture.root.clone()).unwrap();
        recipe.program = "/bin/cat".into();
        recipe.arguments.clear();
        let spec = ProcessSpec::from(&recipe);
        let mut initial = Step::default();
        let store = chat::runtime::preflight(&spec, &mut initial).unwrap();
        let (entered, pending) = crossbeam_channel::bounded(1);
        let (release, released) = crossbeam_channel::bounded(1);
        let release = Release(release);
        let handle = Handle::start(
            spec,
            Box::new(Delayed {
                entered,
                release: released,
            }),
            initial,
            store,
            None,
            |_| {},
        )
        .unwrap();
        let id = crate::model::TerminalId::new();
        let instance = uuid::Uuid::new_v4();
        let session = {
            let mut state = fixture.host.lock().unwrap();
            let session = chat::Session {
                id,
                launch: chat::Launch {
                    provider: Provider::Claude,
                    recipe,
                    session_id: None,
                },
                proof: Some(state.proof().clone()),
                terminal: None,
                native_continuity: Default::default(),
                owned_codex_listener: None,
            };
            state.chats.insert(
                id,
                super::super::chat::Record {
                    execution_instance: instance,
                    console_recipe: None,
                    paired_terminal: None,
                    session: session.clone(),
                    handle: handle.clone(),
                    source_process: None,
                    subscribers: BTreeMap::from([(1, false)]),
                    observations: BTreeMap::new(),
                    completion_observed: false,
                    returning_terminal: false,
                },
            );
            session
        };
        let actor = handle.clone();
        let command = thread::spawn(move || actor.command(Command::Interrupt));
        pending.recv_timeout(Duration::from_secs(2)).unwrap();
        let provider = handle
            .provider_process()
            .map(ProviderIncarnation::from_process);
        let host = fixture.host.clone();
        let expected = fixture.expected.clone();
        let (closed, closing) = crossbeam_channel::bounded(1);
        let close = thread::spawn(move || {
            let result = super::super::client::process_request(
                &host,
                1,
                ClientRequest::CloseProjectGuarded {
                    project: expected,
                    sessions: vec![SessionClose {
                        source: HostedSession::Chat(session),
                        execution_instance: Some(instance),
                        provider,
                    }],
                },
            );
            closed.send(result).unwrap();
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        while fixture
            .host
            .lock()
            .unwrap()
            .project_removals
            .lock()
            .unwrap()
            .is_empty()
        {
            assert!(Instant::now() < deadline, "scoped removal was not admitted");
            thread::yield_now();
        }
        assert!(
            closing.recv_timeout(Duration::from_millis(50)).is_err(),
            "provider remains blocked until explicitly released"
        );
        let host = fixture.host.clone();
        let unrelated_id = unrelated.source.id();
        let (answered, answering) = crossbeam_channel::bounded(1);
        let input = thread::spawn(move || {
            let listed = super::super::client::process_request(&host, 1, ClientRequest::List);
            let input = super::super::client::process_request(
                &host,
                1,
                ClientRequest::Input {
                    terminal_id: unrelated_id,
                    bytes: b"hello\n".to_vec(),
                },
            );
            answered.send((listed, input)).unwrap();
        });
        let (listed, input_result) = answering
            .recv_timeout(Duration::from_secs(1))
            .expect("unrelated input/list must complete before provider release");
        assert!(listed.is_ok());
        assert!(input_result.is_ok());
        input.join().unwrap();
        let blocked = super::super::client::process_request(
            &fixture.host,
            1,
            ClientRequest::EditWorkspaceModel {
                edit: ModelEdit::ReopenProject {
                    repository: fixture.root.clone(),
                    name: "Retry".into(),
                    kind: ProjectKind::Folder,
                    checkout: None,
                    primary_checkout: None,
                },
            },
        )
        .unwrap_err();
        assert!(blocked.message.contains("being removed"));
        let transfer = ClientRequest::EditWorkspaceModel {
            edit: ModelEdit::ChangeView {
                scope: Scope::Worktree {
                    repository: unrelated_dir.clone(),
                    path: unrelated_dir,
                },
                expected_revision: 0,
                operation: uuid::Uuid::new_v4(),
                change: crate::workspace_model::views::Change::Transfer {
                    id: crate::model::CardId::new(),
                    destination: Scope::Worktree {
                        repository: fixture.root.clone(),
                        path: fixture.root.clone(),
                    },
                    before: None,
                },
            },
        };
        assert!(
            super::super::client::process_request(&fixture.host, 1, transfer)
                .unwrap_err()
                .message
                .contains("being removed")
        );
        assert!(ensure_available(&fixture.host, None, Some(&fixture.root), None).is_err());
        let github =
            crate::github_identity::GitHubRepoKey::new("github.com", None, "fixture", "project")
                .unwrap();
        let cleanup = ClientRequest::DeviceOperation {
            operation: crate::device_service::Operation::RemoveMergedWorktree {
                request: crate::worktree_cleanup::RemovalRequest {
                    repository: fixture.root.clone(),
                    path: fixture.root.join("linked"),
                    branch: "fixture".into(),
                    head_oid: "a".repeat(40),
                    source_repository: github.clone(),
                    pull_request: crate::github_identity::PullRequestKey {
                        repository: github,
                        number: 1,
                    },
                },
            },
        };
        assert!(
            super::super::client::process_request(&fixture.host, 1, cleanup)
                .unwrap_err()
                .message
                .contains("being removed"),
            "the early automatic-cleanup port must refuse before Git or GitHub preparation"
        );
        let save = ClientRequest::DeviceOperation {
            operation: crate::device_service::Operation::Write {
                root: fixture.root.clone(),
                path: "pending.txt".into(),
                expected: String::new(),
                content: "must not save".into(),
            },
        };
        assert!(
            super::super::client::process_request(&fixture.host, 1, save)
                .unwrap_err()
                .message
                .contains("being removed")
        );
        assert!(!fixture.root.join("pending.txt").exists());
        release.0.send(()).unwrap();
        let _ = command.join().unwrap();
        assert!(
            closing
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .is_ok()
        );
        close.join().unwrap();
        assert!(!fixture.handles[0].is_completed());
        assert!(handle.is_completed());
        assert!(
            fixture
                .host
                .lock()
                .unwrap()
                .project_removals
                .lock()
                .unwrap()
                .is_empty()
        );
        assert!(ensure_available(&fixture.host, None, Some(&fixture.root), None).is_ok());
    }

    #[test]
    fn nested_foreign_project_session_is_preserved_and_invalid_roster_has_no_effects() {
        let mut fixture = Fixture::new();
        let own = fixture.spawn(crate::model::TerminalId::new());
        let nested = fixture.root.join("another-project");
        std::fs::create_dir(&nested).unwrap();
        let foreign = fixture.spawn_at(crate::model::TerminalId::new(), &nested);
        {
            let mut state = fixture.host.lock().unwrap();
            state
                .edit_model(
                    1,
                    ModelEdit::OpenProject {
                        repository: nested.clone(),
                        checkout: None,
                        primary_checkout: None,
                        name: "Foreign".into(),
                        kind: ProjectKind::Folder,
                    },
                )
                .unwrap();
            let HostedSession::Terminal(source) = &foreign.source else {
                unreachable!()
            };
            state
                .edit_model(
                    1,
                    ModelEdit::SeedView {
                        scope: Scope::Worktree {
                            repository: nested.clone(),
                            path: nested.clone(),
                        },
                        panes: vec![crate::workspace_model::views::Pane {
                            id: crate::model::CardId::new(),
                            title: "Foreign terminal".into(),
                            user_title: None,
                            pending_terminal_start: false,
                            width_fraction: 0.5,
                            kind: crate::model::CardKind::Terminal(source.clone()),
                            scope: Some(nested.clone()),
                        }],
                        closed: Vec::new(),
                    },
                )
                .unwrap();
        }
        assert!(
            fixture
                .request(fixture.expected.clone(), vec![own.clone(), foreign])
                .is_err()
        );
        assert!(
            fixture.handles.iter().all(|handle| !handle.is_completed()),
            "a nested foreign source rejects the entire roster before the first close"
        );
        let (response, _, _) = fixture
            .request(fixture.expected.clone(), vec![own])
            .unwrap();
        let ServerResponse::WorkspaceModel { model } = response else {
            panic!()
        };
        assert!(model.project(&fixture.root).is_none());
        assert!(model.project(&nested).is_some());
        assert!(fixture.handles[0].is_completed());
        assert!(!fixture.handles[1].is_completed());
    }

    #[test]
    fn authenticated_stalled_holder_census_expires_without_closing_healthy_pane() {
        const STAGE:&str="CAGENTS_STALLED_PROJECT_HOLDER_TEST";
        if std::env::var_os(STAGE).is_none() {
            let runtime=tempfile::tempdir_in("/var/tmp").unwrap();
            let output=std::process::Command::new(std::env::current_exe().unwrap()).args(["--exact","host::project_removal::tests::authenticated_stalled_holder_census_expires_without_closing_healthy_pane","--nocapture"]).env(STAGE,"1").env("XDG_RUNTIME_DIR",runtime.path()).env("XDG_DATA_HOME",runtime.path().join("data")).output().unwrap();
            assert!(output.status.success(),"{}\n{}",String::from_utf8_lossy(&output.stdout),String::from_utf8_lossy(&output.stderr));
            return;
        }
        use crate::daemon::holder_registry::{self as holders,Kind,Process,Record};
        let mut fixture=Fixture::with_proof(HostSessionProof {
            host_session_id:HostSessionId::new("stalled-census"),
            boot_id:crate::daemon::read_boot_id().unwrap(),
            host_incarnation:HostIncarnation::new("original"),
        });
        let healthy=fixture.spawn(crate::model::TerminalId::new());
        let directory=holders::directory(Kind::Native).unwrap();
        crate::secure_file::prepare_private_directory(&directory).unwrap();
        let record=Record { version:3,kind:Kind::Native,application:crate::instance::APPLICATION_ID.into(),boot:crate::daemon::read_boot_id().unwrap(),terminal:crate::model::TerminalId::new(),holder:Process::read(std::process::id() as i32).unwrap(),root:Process::read(std::process::id() as i32).unwrap() };
        let endpoint=holders::socket(&directory,record.terminal);
        let listener=std::os::unix::net::UnixListener::bind(&endpoint).unwrap();
        record.create(&directory).unwrap();
        let (release,wait)=crossbeam_channel::bounded::<()>(1);
        let server=std::thread::spawn(move || { let (_socket,_)=listener.accept().unwrap(); wait.recv_timeout(Duration::from_secs(3)).unwrap(); });
        let reservation=reserve(&fixture.host,&fixture.expected,&[healthy]).unwrap();
        let start=Instant::now();
        let failure=census::require_no_unhosted_owners_until(&fixture.host,&reservation,start+Duration::from_millis(80)).unwrap_err();
        assert!(failure.message.contains("could not be authenticated"),"{failure:?}");
        assert!(start.elapsed()<Duration::from_secs(2));
        assert!(!fixture.handles[0].is_completed());
        assert!(super::super::agent_execution::registry(&fixture.host).unwrap().lock().unwrap().model().unwrap().project(&fixture.root).is_some());
        drop(reservation);
        assert!(fixture.host.lock().unwrap().project_removals.lock().unwrap().is_empty());
        release.send(()).unwrap();
        server.join().unwrap();
        assert!(endpoint.exists());
        assert!(holders::evidence(&endpoint,Kind::Native).unwrap().is_some());
        record.remove(&directory);
    }

    #[test]
    fn authenticated_orphan_holder_refuses_missing_project_before_healthy_close_but_foreign_does_not() {
        const STAGE: &str = "CAGENTS_PROJECT_ORPHAN_CENSUS_TEST";
        if std::env::var_os(STAGE).is_none() {
            let runtime = tempfile::tempdir_in("/var/tmp").unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "host::project_removal::tests::authenticated_orphan_holder_refuses_missing_project_before_healthy_close_but_foreign_does_not", "--nocapture"])
                .env(STAGE,"1").env("XDG_RUNTIME_DIR",runtime.path()).env("XDG_DATA_HOME",runtime.path().join("data")).output().unwrap();
            assert!(output.status.success(), "{}\n{}",String::from_utf8_lossy(&output.stdout),String::from_utf8_lossy(&output.stderr));
            return;
        }
        struct Retained(crate::chat::native_holder::Detached);
        impl Drop for Retained { fn drop(&mut self) { let _ = self.0.close(); } }
        for foreign in [false,true] {
            let mut source = crate::chat::native_holder::fixture_source();
            let boot = crate::daemon::read_boot_id().unwrap();
            source.proof.as_mut().unwrap().boot_id = boot.clone();
            source.owned_codex_listener.as_mut().unwrap().allocator.boot_id = boot;
            let proof = source.proof.clone().unwrap();
            let paths = crate::daemon::RuntimePaths::discover().unwrap();
            let _lease = crate::daemon::HostLease::acquire(&paths,&proof).unwrap();
            let mut fixture = Fixture::with_proof(proof);
            let healthy = fixture.spawn(crate::model::TerminalId::new());
            let directory = if foreign { let path=fixture._directory.path().join("foreign"); std::fs::create_dir(&path).unwrap(); path } else { fixture.root.clone() };
            source.launch.recipe.working_directory = directory.clone();
            if foreign {
                let mut state=fixture.host.lock().unwrap();
                state.edit_model(1,ModelEdit::OpenProject { repository:directory.clone(),checkout:None,primary_checkout:None,name:"Foreign".into(),kind:ProjectKind::Folder }).unwrap();
                state.edit_model(1,ModelEdit::SeedView { scope:Scope::Worktree { repository:directory.clone(),path:directory.clone() },panes:vec![crate::workspace_model::views::Pane { id:crate::model::CardId::new(),title:"Retained foreign native".into(),user_title:None,pending_terminal_start:false,width_fraction:0.5,kind:crate::model::CardKind::Chat(source.clone()),scope:Some(directory.clone()) }],closed:vec![] }).unwrap();
            }
            let context = Arc::new(crate::chat::protocol::ExecutionContext::capture(&directory).unwrap());
            let spec = crate::chat::protocol::ProcessSpec {
                execution:Some(context.clone()),messages:None,
                transport:crate::chat::protocol::TransportSpec::OwnedUnixWebSocket { socket:source.owned_codex_listener.as_ref().unwrap().socket.clone() },
                program:"/bin/sleep".into(),arguments:vec!["60".into()],working_directory:directory,
                environment:Default::default(),unset_environment:Default::default(),
            };
            let mut holder = crate::chat::native_holder::spawn(spec,source).unwrap();
            context.installed();
            let record=holder.record().clone();
            holder.confirm(uuid::Uuid::new_v4().to_string(), crate::chat::protocol::ServerIdentity {
                user_agent:"cagents/0.157.1 (Linux)".into(),
                canonical_home:fixture._directory.path().join("codex-home"),
                reported_home:fixture._directory.path().join("codex-home").to_string_lossy().into_owned(),
            },record.root.clone()).unwrap();
            let retained=Retained(holder.preserve());
            assert!(record.root.is_current());
            if !foreign { std::fs::rename(&fixture.root,fixture._directory.path().join("moved")).unwrap(); }
            // No saved/native adoption failure map or installed chat exists:
            // the authenticated orphan census is the only ownership evidence.
            assert!(fixture.host.lock().unwrap().chats.is_empty());
            let result=fixture.request(fixture.expected.clone(),vec![healthy]);
            if foreign {
                assert!(result.is_ok(),"{result:?}");
                assert!(fixture.handles[0].is_completed());
            } else {
                let failure=result.unwrap_err();
                assert!(failure.message.contains("Retained session"),"{failure:?}");
                assert!(!fixture.handles[0].is_completed());
                let model=super::super::agent_execution::registry(&fixture.host).unwrap().lock().unwrap().model().unwrap();
                assert!(model.project(&fixture.root).is_some());
            }
            assert!(record.root.is_current(),"census must not adopt or terminate an orphan provider");
            assert!(fixture.host.lock().unwrap().project_removals.lock().unwrap().is_empty());
            drop(retained);
            assert!(!record.root.is_current());
        }
    }

    #[test]
    fn known_unhosted_native_owner_refuses_before_healthy_close_and_foreign_owner_does_not() {
        use crate::{agent::Provider, chat::{self, Launch}, upgrade::handoff::Chat};
        for foreign in [false,true] {
            let mut fixture = Fixture::new(); let healthy = fixture.spawn(crate::model::TerminalId::new());
            let root = if foreign { let path=fixture._directory.path().join("foreign"); std::fs::create_dir(&path).unwrap(); path } else { fixture.root.clone() };
            let thread=uuid::Uuid::new_v4().to_string();
            let mut recipe=LaunchRecipe::shell(root.clone()).unwrap(); recipe.program="claude".into(); recipe.arguments=vec!["--resume".into(),thread.clone()];
            let source = chat::Session { id:crate::model::TerminalId::new(), launch:Launch { provider:Provider::Claude,session_id:Some(thread),recipe },proof:Some(fixture.host.lock().unwrap().proof().clone()),terminal:None,native_continuity:Default::default(),owned_codex_listener:None };
            {
                let mut state=fixture.host.lock().unwrap();
                if foreign { state.edit_model(1,ModelEdit::OpenProject {repository:root.clone(),checkout:None,primary_checkout:None,name:"Foreign".into(),kind:ProjectKind::Folder}).unwrap(); }
                state.edit_model(1,ModelEdit::SeedView {scope:Scope::Worktree {repository:root.clone(),path:root.clone()},panes:vec![crate::workspace_model::views::Pane {id:crate::model::CardId::new(),title:"Retained native".into(),user_title:None,pending_terminal_start:false,width_fraction:0.5,kind:crate::model::CardKind::Chat(source.clone()),scope:Some(root.clone())}],closed:vec![]}).unwrap();
                state.native_adoption_failures.remember_upgrade(&Chat {launch:source.launch.clone(),source:source.clone(),previous_sources:vec![]},"Retained provider history cannot be read");
                assert!(!state.chats.contains_key(&source.id));
            }
            let result=fixture.request(fixture.expected.clone(),vec![healthy]);
            if foreign { assert!(result.is_ok()); assert!(fixture.handles[0].is_completed()); }
            else { assert!(result.unwrap_err().message.contains("unresolved provider ownership")); assert!(!fixture.handles[0].is_completed()); assert!(fixture.host.lock().unwrap().read_model().unwrap().project(&fixture.root).is_some()); }
            assert!(fixture.host.lock().unwrap().native_adoption_failures.contains_id(source.id),"the uncertain retained owner must remain reachable for its own explicit cleanup");
            assert!(fixture.host.lock().unwrap().project_removals.lock().unwrap().is_empty());
        }
    }

    #[test]
    fn unrelated_valid_source_refuses_the_entire_project_roster() {
        let mut fixture = Fixture::new();
        let own = fixture.spawn(crate::model::TerminalId::new());
        let unrelated_dir = fixture._directory.path().join("unrelated");
        std::fs::create_dir(&unrelated_dir).unwrap();
        let unrelated = fixture.spawn_at(crate::model::TerminalId::new(), &unrelated_dir);
        let error = fixture
            .request(fixture.expected.clone(), vec![own, unrelated])
            .unwrap_err();
        assert!(error.message.contains("does not belong"));
        assert!(
            fixture.handles.iter().all(|handle| !handle.is_completed()),
            "qualification precedes every close effect"
        );
        assert!(
            fixture
                .host
                .lock()
                .unwrap()
                .project_removals
                .lock()
                .unwrap()
                .is_empty()
        );
        assert!(
            fixture
                .host
                .lock()
                .unwrap()
                .read_model()
                .unwrap()
                .project(&fixture.root)
                .is_some()
        );
    }

    #[test]
    fn exact_completed_native_cleanup_never_attaches_or_spawns_a_provider() {
        use crate::{
            agent::Provider,
            chat::{
                self, Command,
                protocol::{Adapter, ProcessSpec, Step},
                runtime::Handle,
            },
        };
        use std::collections::BTreeMap;
        struct Idle;
        impl Adapter for Idle {
            fn initialize(&mut self) -> Result<Step, String> {
                Ok(Step::default())
            }
            fn command(&mut self, _: Command) -> Result<Step, String> {
                Ok(Step::default())
            }
            fn receive(&mut self, _: serde_json::Value) -> Result<Step, String> {
                Ok(Step::default())
            }
        }
        let fixture = Fixture::new();
        let counter = fixture._directory.path().join("starts");
        let mut recipe = LaunchRecipe::shell(fixture.root.clone()).unwrap();
        recipe.program = "/bin/sh".into();
        recipe.arguments = vec![
            "-c".into(),
            "printf 'started\n' >> \"$1\"; exec /bin/cat".into(),
            "fixture".into(),
            counter.to_string_lossy().into_owned(),
        ];
        let spec = ProcessSpec::from(&recipe);
        let mut initial = Step::default();
        let store = chat::runtime::preflight(&spec, &mut initial).unwrap();
        let actor = Handle::start(spec, Box::new(Idle), initial, store, None, |_| {}).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !counter.exists() {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        actor.close_for_resume().unwrap();
        assert!(actor.is_completed());
        let instance = uuid::Uuid::new_v4();
        let id = crate::model::TerminalId::new();
        let source = {
            let mut state = fixture.host.lock().unwrap();
            let source = chat::Session {
                id,
                launch: chat::Launch {
                    provider: Provider::Claude,
                    recipe,
                    session_id: None,
                },
                proof: Some(state.proof().clone()),
                terminal: None,
                native_continuity: Default::default(),
                owned_codex_listener: None,
            };
            state.chats.insert(
                id,
                super::super::chat::Record {
                    execution_instance: instance,
                    console_recipe: None,
                    paired_terminal: None,
                    session: source.clone(),
                    handle: actor.clone(),
                    source_process: None,
                    subscribers: BTreeMap::new(),
                    observations: BTreeMap::new(),
                    completion_observed: true,
                    returning_terminal: false,
                },
            );
            source
        };
        let provider = ProviderIncarnation::from_process(
            actor
                .provider_process()
                .expect("the completed actor retains its last provider incarnation"),
        );
        let missing = SessionClose {
            source: HostedSession::Chat(source.clone()),
            execution_instance: Some(instance),
            provider: None,
        };
        assert!(
            fixture
                .request(fixture.expected.clone(), vec![missing])
                .unwrap_err()
                .message
                .contains("cannot be proved"),
            "None must never equal absent provider authority"
        );
        let mut stale = provider.clone();
        stale.start = stale.start.wrapping_add(1);
        assert!(
            super::super::chat::close_project_session(
                &fixture.host,
                1,
                &source,
                instance,
                &actor,
                &stale,
                Instant::now() + Duration::from_secs(2)
            )
            .is_err()
        );
        assert!(
            fixture.host.lock().unwrap().chats.contains_key(&id),
            "a stale backend incarnation must preserve even a completed successor actor"
        );
        super::super::chat::close_project_session(
            &fixture.host,
            1,
            &source,
            instance,
            &actor,
            &provider,
            Instant::now() + Duration::from_secs(2),
        )
        .unwrap();
        assert_eq!(std::fs::read_to_string(&counter).unwrap(), "started\n");
        assert!(!fixture.host.lock().unwrap().chats.contains_key(&id));
    }

    #[test]
    fn stale_project_and_registry_incarnations_refuse_before_teardown() {
        let mut fixture = Fixture::new();
        let selected = fixture.spawn(crate::model::TerminalId::new());
        {
            let mut state = fixture.host.lock().unwrap();
            state
                .edit_model(
                    1,
                    ModelEdit::CloseProject {
                        repository: fixture.root.clone(),
                    },
                )
                .unwrap();
            state
                .edit_model(
                    1,
                    ModelEdit::OpenProject {
                        repository: fixture.root.clone(),
                        checkout: None,
                        primary_checkout: None,
                        name: "Replacement".into(),
                        kind: ProjectKind::Folder,
                    },
                )
                .unwrap();
        }
        assert!(
            fixture
                .request(fixture.expected.clone(), vec![selected.clone()])
                .is_err()
        );
        assert!(
            !fixture.handles[0].is_completed(),
            "reopened path must preserve its live process before any close"
        );
        let replacement_path = fixture._directory.path().join("recreated.sqlite3");
        let mut recreated = Registry::at(&replacement_path).unwrap();
        recreated
            .apply(&ModelEdit::OpenProject {
                repository: fixture.root.clone(),
                checkout: None,
                primary_checkout: None,
                name: "Recreated".into(),
                kind: ProjectKind::Folder,
            })
            .unwrap();
        let model = recreated.model().unwrap();
        assert_eq!(model.projects[0].ordinal, fixture.expected.ordinal);
        fixture.host.lock().unwrap().install_model(recreated);
        assert!(
            fixture
                .request(fixture.expected.clone(), vec![selected.clone()])
                .is_err()
        );
        assert!(
            !fixture.handles[0].is_completed(),
            "same rank in a recreated registry cannot authorize teardown"
        );
        let valid = ProjectInstance {
            registry: model.identity,
            repository: fixture.root.clone(),
            ordinal: model.projects[0].ordinal,
        };
        let (response, _, _) = fixture.request(valid, vec![selected]).unwrap();
        let ServerResponse::WorkspaceModel { model } = response else {
            panic!()
        };
        assert!(model.project(&fixture.root).is_none());
        assert!(fixture.handles[0].is_completed());
    }
    #[test]
    fn changed_worker_or_new_session_refuses_the_entire_roster_before_first_close() {
        let mut fixture = Fixture::new();
        let first = fixture.spawn(crate::model::TerminalId::new());
        let second = fixture.spawn(crate::model::TerminalId::new());
        assert!(
            fixture
                .request(fixture.expected.clone(), vec![first.clone()])
                .is_err()
        );
        assert!(
            fixture
                .host
                .lock()
                .unwrap()
                .project_removals
                .lock()
                .unwrap()
                .is_empty(),
            "failed preflight releases its scoped reservation"
        );
        assert!(
            fixture.handles.iter().all(|handle| !handle.is_completed()),
            "a missing new session must not partially close the earlier roster"
        );
        fixture.handles[1].shutdown().unwrap();
        fixture
            .host
            .lock()
            .unwrap()
            .retire_terminal(second.source.id());
        let successor = fixture.spawn(second.source.id());
        assert_eq!(
            second.source, successor.source,
            "full recipe and daemon proof do not identify the worker"
        );
        assert_ne!(second.execution_instance, successor.execution_instance);
        assert!(
            fixture
                .request(fixture.expected.clone(), vec![first.clone(), second])
                .is_err()
        );
        assert!(!fixture.handles[0].is_completed());
        assert!(!fixture.handles[2].is_completed());
        fixture
            .request(fixture.expected.clone(), vec![first, successor])
            .unwrap();
        assert!(fixture.handles.iter().all(TerminalHandle::is_completed));
    }
}
