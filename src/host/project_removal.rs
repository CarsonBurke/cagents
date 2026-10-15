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
pub(super) mod custody;
mod census;
use reservation::Reservation;
pub(super) use reservation::{
    Reservations, admit, ensure_agent_available, ensure_available, ensure_prepared_edit_available, reserve, reserve_relocation,
};

const MAX_SESSIONS: usize = 4096;
fn error(message: impl Into<String>) -> ProtocolError {
    ProtocolError::new(ProtocolErrorCode::InvalidRequest, message, false)
}
#[derive(Clone)]
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

pub(super) fn prepare_removal(
    host: &SharedHost,
    client: ClientId,
    project: ProjectInstance,
    sessions: Vec<SessionClose>,
    reservation: &Reservation,
) -> Result<PreparedRemoval, ProtocolError> {
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
    Ok(PreparedRemoval{project,sessions,owners,next:0})
}

pub(super) struct PreparedRemoval {
    project: ProjectInstance,
    sessions: Vec<SessionClose>,
    owners: Vec<(Owner,HostedSession)>,
    next: usize,
}
impl PreparedRemoval {
    pub(super) fn advance(&mut self,host:&SharedHost,client:ClientId,authority:&custody::CommitAuthority<'_>)->Result<crate::workspace_model::WorkspaceModel,ProtocolError> {
        if authority.model_is_committed(host)? {return host.lock().unwrap_or_else(|p|p.into_inner()).read_model();}
        let registry=super::agent_execution::registry(host).map_err(error)?;
        authority.before_close(host)?;
    let gate = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .control_gate
        .clone();
    let deadline = Instant::now() + Duration::from_secs(60);
    while self.next<self.sessions.len() {
        let selected=self.sessions[self.next].clone();
        let (owner,_)=self.owners[self.next].clone();
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
            .require_project_instance(&self.project)
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
                        self.next+=1;
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
                        self.next+=1;
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
        self.next+=1;
    }
    authority.pending_commit(host)?;
    #[cfg(test)] authority.before_final_commit(host);
    let _commit = super::client::try_scoped_control_write(&gate)?;
    super::client::ensure_control_open(host)?;
    let mut state=host.lock().unwrap_or_else(|p|p.into_inner());
    state.apply_project_operation(client,&ModelEdit::CloseProjectInstance{project:self.project.clone()},&[],&[],authority)
    }
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
    fn identified(request:ClientRequest)->ClientRequest {
        use crate::project_recovery::{ProjectOperationRequest,ProjectOperationAction};
        match request {
            ClientRequest::CloseProjectGuarded{project,sessions}=>ClientRequest::ProjectOperation{request:ProjectOperationRequest{operation:uuid::Uuid::new_v4(),project,action:ProjectOperationAction::Remove{sessions}}},
            ClientRequest::EditWorkspaceModel{edit:ModelEdit::RelocateProject{project,relocation}}=>ClientRequest::ProjectOperation{request:ProjectOperationRequest{operation:uuid::Uuid::new_v4(),project,action:ProjectOperationAction::Locate{relocation}}},
            request=>request,
        }
    }
    fn fixture_request(host:&SharedHost,client:ClientId,request:ClientRequest)->Result<(ServerResponse,Vec<ServerMessage>,bool),ProtocolError> {
        let (response,messages,close)=super::super::client::process_request(host,client,identified(request))?;
        match response {
            ServerResponse::ProjectOperation{outcome:crate::project_recovery::ProjectOperationOutcome::Committed{model,..}}=>Ok((ServerResponse::WorkspaceModel{model},messages,close)),
            ServerResponse::ProjectOperation{outcome:crate::project_recovery::ProjectOperationOutcome::Pending{reason,..}}=>Err(ProtocolError::new(ProtocolErrorCode::Internal,reason,true)),
            response=>Ok((response,messages,close)),
        }
    }
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
            fixture_request(
                &self.host,
                1,
                ClientRequest::CloseProjectGuarded {
                    project: expected,
                    sessions,
                },
            )
        }
    }
    fn isolated_custody(test:&str)->bool {
        const STAGE:&str="CAGENTS_PROJECT_CUSTODY_CHILD";
        if std::env::var(STAGE).ok().as_deref()==Some(test) {return false;}
        use crate::repository::{CancellationToken,CommandSpec,ProcessExecutor,SystemProcessExecutor};
        let runtime=tempfile::tempdir().unwrap();
        for name in ["data","config"] {std::fs::create_dir(runtime.path().join(name)).unwrap();}
        let command=CommandSpec::new(std::env::current_exe().unwrap())
            .args(["--exact",test,"--nocapture","--test-threads=1"])
            .env(STAGE,test).env("XDG_RUNTIME_DIR",runtime.path())
            .env("XDG_DATA_HOME",runtime.path().join("data")).env("XDG_CONFIG_HOME",runtime.path().join("config"))
            .timeout(Duration::from_secs(45)).output_limits(2*1024*1024,2*1024*1024);
        let output=SystemProcessExecutor.execute(&command,&CancellationToken::default()).unwrap();
        let stdout=String::from_utf8_lossy(&output.stdout);
        assert!(output.success&&stdout.contains("1 passed; 0 failed;"),"isolated actual custody case did not execute/pass: {}\n{}",stdout,String::from_utf8_lossy(&output.stderr));
        true
    }
    struct HeldFinal {
        start:crossbeam_channel::Sender<()>,
        release:crossbeam_channel::Sender<()>,
        thread:Option<std::thread::JoinHandle<()>>,
    }
    impl Drop for HeldFinal {
        fn drop(&mut self) {
            let _=self.start.try_send(());let _=self.release.try_send(());
            if let Some(thread)=self.thread.take() {let _=thread.join();}
        }
    }
    fn hold_final_commit(host:&SharedHost,operation:uuid::Uuid)->HeldFinal {
        use std::sync::atomic::{AtomicBool,Ordering};
        let gate=super::super::client::control_gate(host);
        let (start,started)=crossbeam_channel::bounded(1);
        let (acquired,ready)=crossbeam_channel::bounded(1);
        let (release,released)=crossbeam_channel::bounded(1);
        let thread=std::thread::spawn(move|| {
            started.recv_timeout(Duration::from_secs(30)).unwrap();
            let _reader=gate.read().unwrap();acquired.send(()).unwrap();
            released.recv_timeout(Duration::from_secs(10)).unwrap();
        });
        let signal=start.clone();let fired=AtomicBool::new(false);
        host.lock().unwrap().project_operation_before_commit=Some(Arc::new(move|current| {
            if current==operation && !fired.swap(true,Ordering::AcqRel) {
                signal.send(()).unwrap();ready.recv_timeout(Duration::from_secs(5)).unwrap();
            }
        }));
        HeldFinal{start,release,thread:Some(thread)}
    }
    fn operation_reply(host:&SharedHost,request:crate::project_recovery::ProjectOperationRequest)->crate::project_recovery::ProjectOperationOutcome {
        let (ServerResponse::ProjectOperation{outcome},_,_)=super::super::client::process_request(host,1,ClientRequest::ProjectOperation{request}).unwrap() else {panic!("typed project operation reply required")};outcome
    }
    #[test]
    fn repaired_git_busy_retains_exact_operation_plan_and_restart_negative_scope() {
        if isolated_custody("host::project_removal::tests::repaired_git_busy_retains_exact_operation_plan_and_restart_negative_scope") {return;}
        use crate::project_recovery::{ProjectOperationRequest,ProjectOperationAction,ProjectOperationOutcome,ProjectOperationPhase};
        use crate::repository::{CancellationToken,CommandSpec,ProcessExecutor,SystemProcessExecutor};
        use std::os::unix::fs::MetadataExt;
        let mut fixture=Fixture::new();
        let other=fixture._directory.path().join("other");std::fs::create_dir(&other).unwrap();
        let unrelated=fixture.spawn_at(crate::model::TerminalId::new(),&other);
        let old=fixture.root.clone();let moved=fixture._directory.path().join("moved");
        let common=fixture._directory.path().join("metadata.git");
        let git=|directory:&std::path::Path,args:Vec<String>| {
            let command=crate::project_recovery::git_repair::fixture_git_command(directory,args);
            SystemProcessExecutor.execute(&command,&CancellationToken::default()).unwrap().require_success(&command.program).unwrap()
        };
        git(fixture._directory.path(),vec!["init".into(),"--separate-git-dir".into(),common.to_string_lossy().into_owned(),old.to_string_lossy().into_owned()]);
        git(&old,vec!["config".into(),"core.worktree".into(),old.to_string_lossy().into_owned()]);
        {
            let mut state=fixture.host.lock().unwrap();
            state.edit_model(1,ModelEdit::CloseProject{repository:old.clone()}).unwrap();
            let model=state.edit_model(1,ModelEdit::OpenProject{repository:common.clone(),checkout:Some(old.clone()),primary_checkout:Some(old.clone()),name:"git".into(),kind:ProjectKind::Git}).unwrap();
            fixture.expected=ProjectInstance{registry:model.identity,repository:common.clone(),ordinal:model.project(&common).unwrap().ordinal};
        }
        std::fs::rename(&old,&moved).unwrap();
        let request=ProjectOperationRequest{operation:uuid::Uuid::new_v4(),project:fixture.expected.clone(),action:ProjectOperationAction::Locate{relocation:crate::project_recovery::Relocation{repository:common.clone(),destination:common.clone(),old_root:old.clone(),new_root:moved.clone()}}};
        let held=hold_final_commit(&fixture.host,request.operation);
        assert!(matches!(operation_reply(&fixture.host,request.clone()),ProjectOperationOutcome::Pending{phase:ProjectOperationPhase::PendingCommit,..}));
        let after=std::fs::read(common.join("config")).unwrap();let inode=std::fs::metadata(common.join("config")).unwrap().ino();
        assert!(String::from_utf8_lossy(&after).contains(moved.to_str().unwrap()),"real Git config repair preceded final busy");
        let model=fixture.host.lock().unwrap().read_model().unwrap();
        assert_eq!(model.project(&common).unwrap().checkout.as_ref(),Some(&old));
        assert_eq!(fixture.host.lock().unwrap().project_removals.lock().unwrap().len(),1);
        assert!(ensure_available(&fixture.host,Some(&common),Some(&moved),None).is_err());
        assert!(ensure_available(&fixture.host,None,Some(&other),None).is_ok());
        assert!(super::super::client::process_request(&fixture.host,1,ClientRequest::Input{terminal_id:unrelated.source.id(),bytes:b"unrelated remains live\n".to_vec()}).is_ok());
        // A new daemon can retain only the actual durable negative scope. It
        // cannot recreate the prepared root/admin pins or replay the repair.
        let mut restarted=HostState::new(fixture.host.lock().unwrap().proof().clone());
        restarted.install_model(Registry::at(&fixture._directory.path().join("registry.sqlite3")).unwrap());restarted.register_client(1);
        let restarted=Arc::new(HostLock::new(restarted));
        assert!(matches!(operation_reply(&restarted,request.clone()),ProjectOperationOutcome::Pending{phase:ProjectOperationPhase::UnavailableAfterRestart,..}));
        assert!(ensure_available(&restarted,Some(&common),Some(&moved),None).is_err());
        assert!(ensure_available(&restarted,None,Some(&other),None).is_ok());drop(restarted);
        std::fs::write(common.join("config.lock"),b"foreign lock after original repair").unwrap();
        fixture.host.lock().unwrap().project_operation_before_commit=None;drop(held);
        assert!(matches!(operation_reply(&fixture.host,request.clone()),ProjectOperationOutcome::Committed{..}));
        assert_eq!(std::fs::metadata(common.join("config")).unwrap().ino(),inode,"retry cannot repeat a completed Git write");
        assert_eq!(std::fs::read(common.join("config.lock")).unwrap(),b"foreign lock after original repair");
        assert_eq!(std::fs::read(common.join("config")).unwrap(),after);
        let model=fixture.host.lock().unwrap().read_model().unwrap();assert_eq!(model.project(&common).unwrap().checkout.as_ref(),Some(&moved));
        assert!(fixture.host.lock().unwrap().project_removals.lock().unwrap().is_empty());
        assert!(matches!(operation_reply(&fixture.host,request.clone()),ProjectOperationOutcome::Committed{..}),"same operation has an idempotent settled reply");
        let mut changed=request;changed.project.ordinal+=1;
        assert!(super::super::client::process_request(&fixture.host,1,ClientRequest::ProjectOperation{request:changed}).is_err());
        assert!(!fixture.handles[0].is_completed());
    }
    #[test]
    fn closed_owner_before_final_busy_is_not_reclosed_or_reinventoried_on_same_operation_retry() {
        if isolated_custody("host::project_removal::tests::closed_owner_before_final_busy_is_not_reclosed_or_reinventoried_on_same_operation_retry") {return;}
        use crate::project_recovery::{ProjectOperationRequest,ProjectOperationAction,ProjectOperationOutcome,ProjectOperationPhase};
        let mut fixture=Fixture::new();let own=fixture.spawn(crate::model::TerminalId::new());
        let other=fixture._directory.path().join("other");std::fs::create_dir(&other).unwrap();
        let unrelated=fixture.spawn_at(crate::model::TerminalId::new(),&other);
        let request=ProjectOperationRequest{operation:uuid::Uuid::new_v4(),project:fixture.expected.clone(),action:ProjectOperationAction::Remove{sessions:vec![own.clone()]}};
        let held=hold_final_commit(&fixture.host,request.operation);
        assert!(matches!(operation_reply(&fixture.host,request.clone()),ProjectOperationOutcome::Pending{phase:ProjectOperationPhase::PendingCommit,..}));
        assert!(fixture.handles[0].is_completed(),"the actual selected owner closed before busy");
        assert!(!fixture.host.lock().unwrap().terminals.contains_key(&own.source.id()));
        assert!(fixture.host.lock().unwrap().read_model().unwrap().project(&fixture.root).is_some());
        assert!(ensure_available(&fixture.host,Some(&fixture.root),Some(&fixture.root),None).is_err());
        assert!(super::super::client::process_request(&fixture.host,1,ClientRequest::Input{terminal_id:unrelated.source.id(),bytes:b"after pending close\n".to_vec()}).is_ok());
        fixture.host.lock().unwrap().project_operation_before_commit=None;drop(held);
        assert!(matches!(operation_reply(&fixture.host,request.clone()),ProjectOperationOutcome::Committed{..}),"original roster still contains the now-absent owner; success must use retained close progress");
        assert!(fixture.host.lock().unwrap().read_model().unwrap().project(&fixture.root).is_none());
        assert!(fixture.host.lock().unwrap().project_removals.lock().unwrap().is_empty());
        assert!(!fixture.handles[1].is_completed());
        assert!(matches!(operation_reply(&fixture.host,request),ProjectOperationOutcome::Committed{..}));
    }

    #[test]
    fn lost_positive_settlement_reply_releases_only_original_heavy_custody_on_same_operation_retry() {
        if isolated_custody("host::project_removal::tests::lost_positive_settlement_reply_releases_only_original_heavy_custody_on_same_operation_retry") {return;}
        use crate::project_recovery::{ProjectOperationRequest,ProjectOperationAction,ProjectOperationOutcome,ProjectOperationPhase};
        let mut fixture=Fixture::new();let selected=fixture.spawn(crate::model::TerminalId::new());
        let other=fixture._directory.path().join("independent-project");std::fs::create_dir(&other).unwrap();
        let independent={
            let mut state=fixture.host.lock().unwrap();
            let model=state.edit_model(1,ModelEdit::OpenProject{repository:other.clone(),checkout:None,primary_checkout:None,name:"Other".into(),kind:ProjectKind::Folder}).unwrap();
            ProjectInstance{registry:model.identity,repository:other.clone(),ordinal:model.project(&other).unwrap().ordinal}
        };
        let b=ProjectOperationRequest{operation:uuid::Uuid::new_v4(),project:independent,action:ProjectOperationAction::Remove{sessions:Vec::new()}};
        let held=hold_final_commit(&fixture.host,b.operation);
        assert!(matches!(operation_reply(&fixture.host,b.clone()),ProjectOperationOutcome::Pending{phase:ProjectOperationPhase::PendingCommit,..}));
        let (_,original_b)=reservation::retained_operation(&fixture.host,b.operation).unwrap();
        {
            let registry=super::super::agent_execution::registry(&fixture.host).unwrap();
            let entries=fixture.host.lock().unwrap().project_removals.clone();
            assert!(reservation::restore_operations(&registry.lock().unwrap(),&entries).is_err(),"reinstall cannot duplicate a live original owner under its different reservation token");
            assert_eq!(entries.lock().unwrap().len(),1);
            let (_,preserved)=reservation::retained_operation(&fixture.host,b.operation).unwrap();
            assert!(Arc::ptr_eq(&original_b,&preserved));
        }
        fixture.host.lock().unwrap().project_operation_before_commit=None;drop(held);
        let a=ProjectOperationRequest{operation:uuid::Uuid::new_v4(),project:fixture.expected.clone(),action:ProjectOperationAction::Remove{sessions:vec![selected]}};
        let operation=a.operation;
        fixture.host.lock().unwrap().project_operation_after_settlement=Some(Arc::new(move |current| {
            assert_eq!(current,operation);Err("Injected lost completion after actual resolved SQL COMMIT".into())
        }));
        assert!(matches!(operation_reply(&fixture.host,a.clone()),ProjectOperationOutcome::Pending{phase:ProjectOperationPhase::ModelCommitted,..}));
        assert!(fixture.handles[0].is_completed(),"the actual selected terminal already closed");
        let (_,original_a)=reservation::retained_operation(&fixture.host,a.operation).unwrap();
        assert!(original_a.retains_prepared());assert!(!original_a.is_settled());
        let registry=super::super::agent_execution::registry(&fixture.host).unwrap();
        assert_eq!(registry.lock().unwrap().project_operation(a.operation).unwrap().unwrap().phase,super::super::model::project_operations::Phase::Resolved,"this is a real committed receipt, not a simulated server outcome");
        assert!(ensure_available(&fixture.host,Some(&fixture.root),None,None).is_err());
        assert!(ensure_available(&fixture.host,Some(&other),None,None).is_err());
        fixture.host.lock().unwrap().project_operation_after_settlement=None;
        assert!(matches!(operation_reply(&fixture.host,a),ProjectOperationOutcome::Committed{..}));
        assert!(original_a.is_settled());assert!(!original_a.retains_prepared(),"positive replay releases the retained owner roster/FD phase");
        assert!(reservation::retained_operation(&fixture.host,operation).is_none());
        let (_,still_b)=reservation::retained_operation(&fixture.host,b.operation).unwrap();
        assert!(Arc::ptr_eq(&original_b,&still_b));assert!(!still_b.is_settled());assert!(still_b.retains_prepared());
        assert!(ensure_available(&fixture.host,Some(&other),None,None).is_err());
        assert!(ensure_available(&fixture.host,Some(&fixture.root),None,None).is_ok());
        assert!(fixture.host.lock().unwrap().read_model().unwrap().project(&other).is_some());
        assert!(matches!(operation_reply(&fixture.host,b),ProjectOperationOutcome::Committed{..}));
    }

    #[test]
    fn final_prepared_model_edit_refuses_an_alias_retarget_after_admission() {
        use std::os::unix::fs::symlink;
        for repository_alias in [false, true] {
            let fixture = Fixture::new();
            let replay = {
                let mut state = fixture.host.lock().unwrap();
                let scope = Scope::Worktree { repository:fixture.root.clone(),path:fixture.root.clone() };
                let model = state.edit_model(1, ModelEdit::SeedView { scope:scope.clone(),panes:vec![],closed:vec![] }).unwrap();
                let revision = model.views.iter().find(|view|view.scope==scope).unwrap().revision;
                let edit = ModelEdit::ChangeView { scope,expected_revision:revision,operation:uuid::Uuid::new_v4(),change:crate::workspace_model::views::Change::Remove {id:crate::model::CardId::new()} };
                state.edit_model(1, edit.clone()).unwrap();
                edit
            };
            let foreign = fixture._directory.path().join("foreign");
            std::fs::create_dir(&foreign).unwrap();
            let alias = fixture._directory.path().join("candidate");
            symlink(&foreign, &alias).unwrap();
            let gate = fixture.host.lock().unwrap().control_gate.clone();
            let reservation = {
                let _writer = gate.write().unwrap();
                reserve(&fixture.host, &fixture.expected, &[]).unwrap()
            };
            let request = ClientRequest::EditWorkspaceModel {
                edit: ModelEdit::OpenProject {
                    repository: if repository_alias { alias.clone() } else { foreign.clone() },
                    checkout: (!repository_alias).then(|| alias.clone()), primary_checkout: None,
                    name: "Replacement".into(), kind: ProjectKind::Folder,
                },
            };
            let before = fixture.host.lock().unwrap().read_model().unwrap();
            let replayed = fixture_request(&fixture.host, 1, ClientRequest::EditWorkspaceModel {edit:replay}).unwrap();
            let ServerResponse::WorkspaceModel {model:replayed} = replayed.0 else {panic!()};
            assert_eq!(replayed.sequence, before.sequence, "seen operations remain idempotent while reserved");
            {
                let _shared = gate.read().unwrap();
                // These are the actual dispatcher admission and effect halves.
                // External filesystem changes are not fenced by control admission.
                admit(&fixture.host, &request).unwrap();
                std::fs::remove_file(&alias).unwrap();
                symlink(&fixture.root, &alias).unwrap();
                let failure = super::super::client::process_admitted(&fixture.host, 1, request.clone()).unwrap_err();
                assert!(failure.message.contains("being removed"), "{failure:?}");
            }
            let after = fixture.host.lock().unwrap().read_model().unwrap();
            assert_eq!(after.sequence, before.sequence);
            assert_eq!(after.project(&fixture.root), before.project(&fixture.root));
            assert!(after.project(&foreign).is_none());
            // A genuinely foreign edit still passes with the same reservation.
            std::fs::remove_file(&alias).unwrap();
            symlink(&foreign, &alias).unwrap();
            let result = fixture_request(&fixture.host, 1, request).unwrap();
            let ServerResponse::WorkspaceModel { model } = result.0 else { panic!() };
            assert!(model.project(&foreign).is_some());
            assert_eq!(model.project(&fixture.root), before.project(&fixture.root));
            {
                let _writer = gate.write().unwrap();
                let mut wrong = fixture.expected.clone(); wrong.ordinal += 1;
                assert!(reservation.release_for_commit(&wrong).is_err());
                assert!(ensure_available(&fixture.host, Some(&fixture.root), None, None).is_err());
                reservation.release_for_commit(&fixture.expected).unwrap();
                let result = super::super::client::process_admitted(&fixture.host, 1, ClientRequest::EditWorkspaceModel {
                    edit: ModelEdit::CloseProjectInstance { project:fixture.expected.clone() },
                }).unwrap();
                let ServerResponse::WorkspaceModel { model } = result.0 else { panic!() };
                assert!(model.project(&fixture.root).is_none());
                assert!(model.project(&foreign).is_some());
            }
            drop(reservation);
        }
    }

    #[test]
    fn slow_project_teardown_keeps_unrelated_input_responsive_and_releases_reservation() {
        const STAGE: &str="CAGENTS_SCOPED_WRITER_INPUT_CHILD";
        if std::env::var_os(STAGE).is_none() {
            let runtime=tempfile::tempdir().unwrap();
            for name in ["data","config"] {std::fs::create_dir(runtime.path().join(name)).unwrap();}
            use crate::repository::{CancellationToken,CommandSpec,ProcessExecutor,SystemProcessExecutor};
            let command=CommandSpec::new(std::env::current_exe().unwrap())
                .args(["--exact","host::project_removal::tests::slow_project_teardown_keeps_unrelated_input_responsive_and_releases_reservation","--nocapture","--test-threads=1"])
                .env(STAGE,"1").env("XDG_RUNTIME_DIR",runtime.path())
                .env("XDG_DATA_HOME",runtime.path().join("data")).env("XDG_CONFIG_HOME",runtime.path().join("config"))
                .timeout(Duration::from_secs(30)).output_limits(2*1024*1024,2*1024*1024);
            let output=SystemProcessExecutor.execute(&command,&CancellationToken::default()).unwrap();
            let stdout=String::from_utf8_lossy(&output.stdout);
            assert!(output.success && stdout.contains("1 passed; 0 failed;"),"isolated scoped-writer fixture must execute exactly its selected test: {}\n{}",stdout,String::from_utf8_lossy(&output.stderr));
            return;
        }
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
                    .recv_timeout(Duration::from_secs(10))
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
        let unrelated_project={
            let mut state=fixture.host.lock().unwrap();
            let model=state.edit_model(1,ModelEdit::OpenProject{repository:unrelated_dir.clone(),checkout:None,primary_checkout:None,name:"Other".into(),kind:ProjectKind::Folder}).unwrap();
            ProjectInstance{registry:model.identity,repository:unrelated_dir.clone(),ordinal:model.project(&unrelated_dir).unwrap().ordinal}
        };
        // Use actual framed connections, not a direct helper call, for both
        // the scoped writer attempt and the subsequent unrelated Input.
        struct Framed {
            stream: std::os::unix::net::UnixStream,
            server: Option<thread::JoinHandle<()>>,
            proof: HostSessionProof,
            next: u64,
        }
        impl Drop for Framed {
            fn drop(&mut self) {
                let _=self.stream.shutdown(std::net::Shutdown::Both);
                if let Some(server)=self.server.take() {let _=server.join();}
            }
        }
        impl Framed {
            fn new(host: &SharedHost, id: u64) -> Self {
                let proof=host.lock().unwrap().proof().clone();
                let (stream,peer)=std::os::unix::net::UnixStream::pair().unwrap();
                stream.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
                stream.set_write_timeout(Some(Duration::from_secs(1))).unwrap();
                let host=host.clone();let server=thread::spawn(move||super::super::client::serve(peer,id,host));
                let mut client=Self{stream,server:Some(server),proof:proof.clone(),next:1};
                assert!(matches!(client.exchange(ClientRequest::Connect{expected_host:Some(proof),resume:Vec::new()}),ServerMessage::Response{response:ServerResponse::Connected{..}}));
                client
            }
            fn exchange(&mut self, request: ClientRequest) -> ServerMessage {
                use crate::terminal_ipc::{ClientEnvelope,FrameCodec,ServerEnvelope,MAX_CLIENT_FRAME_BYTES,MAX_FRAME_BYTES,PROTOCOL_VERSION};
                let request_id=self.next;self.next+=1;
                FrameCodec::new(MAX_CLIENT_FRAME_BYTES).write(&mut self.stream,&ClientEnvelope{version:PROTOCOL_VERSION,request_id,proof:Some(self.proof.clone()),request:identified(request)}).unwrap();
                let mut codec=FrameCodec::new(MAX_FRAME_BYTES);
                loop {
                    let envelope:ServerEnvelope=codec.read(&mut self.stream).expect("framed request must finish before held provider is released").unwrap();
                    assert_eq!(envelope.proof,self.proof);
                    if envelope.request_id==Some(request_id) {return envelope.message;}
                }
            }
        }
        let mut writer=Framed::new(&fixture.host,2);
        let mut framed_input=Framed::new(&fixture.host,3);
        assert!(matches!(framed_input.exchange(ClientRequest::Attach{terminal_id:unrelated.source.id(),after_generation:0,after_event_sequence:0}),ServerMessage::Response{response:ServerResponse::Attached{..}}));
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
        let admitted_session = session.clone();
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
            let result = fixture_request(
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
            .len() < 1
        {
            assert!(Instant::now() < deadline, "scoped removal was not admitted");
            thread::yield_now();
        }
        assert!(
            closing.recv_timeout(Duration::from_millis(50)).is_err(),
            "provider remains blocked until explicitly released"
        );
        // The reservation is published before census. Wait for its actual
        // teardown reader, so the attempted writer cannot slip in beforehand.
        let gate=super::super::client::control_gate(&fixture.host);
        let deadline=Instant::now()+Duration::from_secs(2);
        loop {
            match gate.try_write() {
                Ok(guard)=>drop(guard),
                Err(std::sync::TryLockError::WouldBlock)=>break,
                Err(std::sync::TryLockError::Poisoned(_))=>panic!("fixture admission gate poisoned"),
            }
            assert!(Instant::now()<deadline,"actual teardown never held shared admission");
            thread::yield_now();
        }
        for request in [
            ClientRequest::CloseProjectGuarded{project:unrelated_project.clone(),sessions:vec![unrelated.clone()]},
            ClientRequest::EditWorkspaceModel{edit:ModelEdit::RelocateProject{project:unrelated_project.clone(),relocation:crate::project_recovery::Relocation{repository:unrelated_dir.clone(),destination:unrelated_dir.clone(),old_root:unrelated_dir.clone(),new_root:unrelated_dir.clone()}}},
            ClientRequest::ChatReturnTerminal{chat_id:admitted_session.id},
            ClientRequest::ChatRecoverAccount{source:admitted_session.clone(),replacement:admitted_session.clone(),idle_generation:Some(1),execution_environment:Default::default()},
            ClientRequest::DeviceOperation{operation:crate::device_service::Operation::AccountLifecycle{operation:crate::account::remote::Operation::Create{provider:Provider::Claude,copy_codex_config:false}}},
        ] {
            let ServerMessage::Error(error)=writer.exchange(request) else {panic!("scoped writer must refuse before effects while teardown is held");};
            assert!(error.retryable && error.message.contains("busy"),"{error:?}");
        }
        assert!(matches!(framed_input.exchange(ClientRequest::Input{terminal_id:unrelated.source.id(),bytes:b"after writer attempt\n".to_vec()}),ServerMessage::Response{response:ServerResponse::Accepted}));
        assert!(closing.try_recv().is_err(),"Input must complete while original provider is still held");
        assert!(!fixture.handles[0].is_completed(),"busy writer cannot close the unrelated selected worker");
        {
            let mut state=fixture.host.lock().unwrap();
            assert_eq!(state.project_removals.lock().unwrap().len(),1,"refused writer cannot leave another reservation");
            let model=state.read_model().unwrap();
            for project in [&unrelated_project] {assert!(model.project(&project.repository).is_some(),"busy final phase must preserve original project row");}
        }
        let host = fixture.host.clone();
        let unrelated_id = unrelated.source.id();
        let (answered, answering) = crossbeam_channel::bounded(1);
        let input = thread::spawn(move || {
            let listed = fixture_request(&host, 1, ClientRequest::List);
            let input = fixture_request(
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
        let blocked = fixture_request(
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
            fixture_request(&fixture.host, 1, transfer)
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
            fixture_request(&fixture.host, 1, cleanup)
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
            fixture_request(&fixture.host, 1, save)
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
