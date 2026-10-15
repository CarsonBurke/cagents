//! Prepare process evidence outside HostState, then atomically relocate logical
//! ownership and its exact sessions. Caller retains a scoped project-location reservation.
use super::{
    model::{SessionDirectoryProof, SessionMove},
    state::{ClientId, SharedHost},
};
use crate::{
    agent_execution::HostedSession,
    chat::runtime::{DirectoryProof, Handle},
    model::{CardKind, TerminalId},
    project_recovery::Relocation,
    terminal::TerminalHandle,
    terminal_ipc::{ProtocolError, ProtocolErrorCode},
    workspace_model::{ModelEdit, WorkspaceModel, views::Scope},
};
use std::{
    collections::BTreeMap,
    path::Path,
    time::{Duration, Instant},
};

#[derive(Clone)]
enum Owner {
    Chat {handle:Handle,instance:uuid::Uuid},
    Terminal(TerminalHandle),
    Unavailable,
}
enum VerifiedDirectory {
    Live(DirectoryProof),
    Completed {identity:crate::agent_execution::DirectoryIdentity,context:std::sync::Arc<crate::chat::protocol::ExecutionContext>},
}
impl VerifiedDirectory {
    fn evidence(&self)->SessionDirectoryProof {
        match self {Self::Live(proof)=>SessionDirectoryProof::Live{identity:proof.identity,process:proof.process.clone()},Self::Completed{identity,..}=>SessionDirectoryProof::Completed{identity:*identity}}
    }
}
struct Prepared {
    movement: SessionMove,
    owner: Owner,
    proof: Option<VerifiedDirectory>,
}

fn error(message: impl Into<String>) -> ProtocolError {
    ProtocolError::new(ProtocolErrorCode::InvalidRequest, message, false)
}
fn issued_directory(source: &HostedSession) -> &Path {
    match source {
        HostedSession::Chat(source) => &source.launch.recipe.working_directory,
        HostedSession::Terminal(source) => &source.launch.working_directory,
    }
}
fn owned(scope: &Scope, model: &WorkspaceModel, relocation: &Relocation) -> bool {
    match scope {
        Scope::Worktree { repository, .. } => *repository == relocation.repository,
        Scope::Channel(id) => model
            .channel(*id)
            .is_some_and(|channel| channel.repository == relocation.repository),
    }
}

/// Capture before any destructive handoff or account switch. The returned
/// context owns an independent FD pin, never the source actor's cleanup owner.
pub(super) fn verified_chat_directory(
    host: &SharedHost,
    source: &crate::chat::Session,
    expected: &Handle,
) -> Result<
    (
        std::path::PathBuf,
        std::sync::Arc<crate::chat::protocol::ExecutionContext>,
    ),
    String,
> {
    use crate::chat::protocol::ExecutionContext;
    use std::sync::Arc;
    let current = {
        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
        let record = state
            .chats
            .get(&source.id)
            .ok_or("The original native provider is no longer hosted")?;
        if !record.handle.same_actor(expected) {
            return Err(
                "The native provider changed before its directory could be captured".into(),
            );
        }
        let instance = record.execution_instance;
        state.observe_chat_directory_thread(source.id, instance)
            .map_err(|error| format!("Cannot retain this provider's confirmed conversation. Check available storage and retry: {error}"))?;
        let record = &state.chats[&source.id];
        let mut current = record.session.clone();
        current.launch.session_id = record
            .handle
            .confirmed_session_id()
            .or(current.launch.session_id);
        current
    };
    let registry = super::agent_execution::registry(host)?;
    let binding = {
        let registry = registry.lock().unwrap_or_else(|p| p.into_inner());
        if !registry.authenticated_session_alias(
            &HostedSession::Chat(source.clone()),
            &HostedSession::Chat(current.clone()),
        )? {
            return Err("The pane does not name this original provider incarnation; refresh its ownership before retrying".into());
        }
        registry.session_location(&HostedSession::Chat(current.clone()))?
    };
    if let Some(reason) = binding
        .as_ref()
        .and_then(|binding| binding.refusal.as_ref())
    {
        return Err(reason.clone());
    }
    let directory = binding
        .as_ref()
        .map(|binding| binding.directory.clone())
        .unwrap_or_else(|| current.launch.recipe.working_directory.clone());
    let context = if expected.is_completed() {
        if binding
            .as_ref()
            .and_then(|binding| binding.approved)
            .is_none()
        {
            return Err("The completed source has no verified operational directory binding; create a new session explicitly".into());
        }
        expected.pinned_execution_directory(&directory)?
    } else {
        let proof = expected.capture_execution_directory(&directory)?;
        Arc::new(ExecutionContext::new(Some(proof.directory)))
    };
    if let Some(approved) = binding.as_ref().and_then(|binding| binding.approved)
        && context.directory_identity()? != Some(approved)
    {
        return Err(
            "The original provider directory no longer matches its approved identity".into(),
        );
    }
    let state = host.lock().unwrap_or_else(|p| p.into_inner());
    if !state.chats.get(&source.id).is_some_and(|record| {
        record.handle.same_actor(expected)
            && HostedSession::Chat(record.session.clone())
                .same_process(&HostedSession::Chat(current))
    }) {
        return Err("The native provider changed while its directory was being captured; retry without stopping it".into());
    }
    Ok((directory, context))
}

pub(super) fn prepare_relocation(
    host: &SharedHost,
    origin: ClientId,
    project: crate::project_recovery::ProjectInstance,
    relocation: Relocation,
) -> Result<PreparedRelocation, ProtocolError> {
    let edit = ModelEdit::RelocateProject { project:project.clone(), relocation }
        .canonicalized()
        .map_err(error)?;
    let ModelEdit::RelocateProject { relocation, .. } = &edit else {
        unreachable!()
    };
    let registry = super::agent_execution::registry(host).map_err(error)?;
    let (kind,worktrees)={
        let registry=registry.lock().unwrap_or_else(|p|p.into_inner());
        registry.require_project_relocation(&project,relocation).map_err(error)?;
        let model=registry.model().map_err(error)?;
        let current=model.project(&project.repository).ok_or_else(||error("The original project is no longer open"))?;
        let worktrees=model.views.iter().filter_map(|view|match &view.scope { Scope::Worktree{repository,path} if *repository==project.repository=>Some(path.clone()),_=>None }).collect::<Vec<_>>();
        (current.kind,worktrees)
    };
    // Retain root/admin authority before effects. All provider/agent proofs
    // below are prepared before this plan's first metadata publication.
    let repair = if kind==crate::model::ProjectKind::Git && !relocation.verification_only() {
        crate::project_recovery::git_repair::RepairPlan::prepare(relocation,&worktrees)
    } else {
        crate::project_recovery::git_repair::RepairPlan::pin(relocation)
    }.map_err(error)?;
    let sources = {
        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
        let model = state.read_model()?;
        let mut sources = BTreeMap::<TerminalId, HostedSession>::new();
        for view in &model.views {
            if !owned(&view.scope, &model, relocation) {
                continue;
            }
            for pane in &view.panes {
                match &pane.kind {
                    CardKind::Chat(session)
                        if state.chats.contains_key(&session.id)
                            || !session.launch.needs_read_only_history() =>
                    {
                        sources.insert(session.id, HostedSession::Chat(session.clone()));
                    }
                    CardKind::Terminal(session) if session.continuity.live_proof().is_some() => {
                        sources
                            .entry(session.id)
                            .or_insert_with(|| HostedSession::Terminal(session.clone()));
                    }
                    _ => {}
                }
            }
        }
        for agent in &model.agents {
            let repository = model
                .agent_locations
                .iter()
                .find(|location| location.agent == agent.id)
                .map(|location| &location.repository)
                .unwrap_or(&agent.spec.target.repository);
            if *repository == relocation.repository
                && let Some(source) = &agent.session
            {
                sources.insert(source.id(), source.clone());
            }
        }
        // Explicit verification must also recover a metadata write that
        // failed on the last provider event while the actor remained idle.
        let instances = sources
            .iter()
            .filter_map(|(id, source)| match source {
                HostedSession::Chat(_) => state
                    .chats
                    .get(id)
                    .map(|record| (*id, record.execution_instance)),
                HostedSession::Terminal(_) => None,
            })
            .collect::<Vec<_>>();
        for (id, instance) in instances {
            state.observe_chat_directory_thread(id, instance)
                .map_err(|reason| error(format!("Cannot retain the provider's confirmed conversation. Check available storage and retry: {reason}")))?;
        }
        // A numeric ID locates a possible owner; exact source or authenticated
        // retained lineage must still prove the association outside this lock.
        sources
            .into_iter()
            .map(|(id, source)| {
                let owner = if let HostedSession::Chat(_) = &source
                    && let Some(record) = state.chats.get(&id)
                {
                    let mut current = record.session.clone();
                    current.launch.session_id = record
                        .handle
                        .confirmed_session_id()
                        .or(current.launch.session_id);
                    (
                        source,
                        HostedSession::Chat(current),
                        Owner::Chat{handle:record.handle.clone(),instance:record.execution_instance},
                    )
                } else if let HostedSession::Terminal(_) = &source
                    && let Some(record) = state.terminals.get(&id)
                {
                    // Raw records retain launch continuity. Only HostState projects
                    // the authenticated current daemon proof for a live worker.
                    state.descriptor(0, id).map(|descriptor| {
                        (
                            source,
                            HostedSession::Terminal(descriptor.session),
                            Owner::Terminal(record.handle.clone()),
                        )
                    })?
                } else {
                    (source.clone(), source, Owner::Unavailable)
                };
                Ok(owner)
            })
            .collect::<Result<Vec<_>, ProtocolError>>()?
    };
    if sources.len()>1024 {return Err(error("Locate exceeds its retained session-proof capacity; no metadata repair started"));}
    let chat_gates=sources.iter().filter_map(|(_,source,_)|matches!(source,HostedSession::Chat(_)).then(||super::chat::transition_gate(host,source.id())))
        .collect::<Vec<_>>();
    let transitions=chat_gates.iter().map(|gate|match gate.try_lock() {
        Ok(guard)=>Ok(guard),Err(std::sync::TryLockError::Poisoned(poison))=>Ok(poison.into_inner()),
        Err(std::sync::TryLockError::WouldBlock)=>Err(error("A conversation lifecycle transition is in progress; no metadata repair started")),
    }).collect::<Result<Vec<_>,ProtocolError>>()?;
    let agent_inputs = registry
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .agent_relocation_inputs(relocation)
        .map_err(error)?;
    if sources.len().saturating_add(agent_inputs.len())>1024 {return Err(error("Locate exceeds its retained owner-proof capacity; no metadata repair started"));}
    let agent_moves = agent_inputs
        .into_iter()
        .map(|input| input.verify())
        .collect::<Vec<_>>();
    let mut prepared = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    for (cached, source, owner) in sources {
        if !matches!(owner, Owner::Unavailable)
            && !registry
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .authenticated_session_alias(&cached, &source)
                .map_err(error)?
        {
            return Err(error(
                "A saved pane names a different provider incarnation or issued launch. Refresh this pane from its original owner before locating its project; no session was moved",
            ));
        }
        let previous = registry
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .session_location(&source)
            .map_err(error)?;
        let directory = previous
            .as_ref()
            .map(|location| {
                location
                    .blocked_directory
                    .as_deref()
                    .unwrap_or(&location.directory)
            })
            .unwrap_or_else(|| issued_directory(&source));
        let candidate = relocation.mapped(directory);
        let approved = previous.as_ref().and_then(|location| location.approved);
        let proof = if Instant::now() >= deadline {
            Err("Locate reached its provider verification time limit; retry after these providers are responsive, or close the affected panes explicitly".into())
        } else { match &owner {
            Owner::Chat{handle,..}=>handle.refresh_retained_state().and_then(|()| {
                if handle.is_completed() {
                    let expected=approved.ok_or("The completed provider has no original approved directory binding; close it explicitly instead of granting a replacement folder".to_owned())?;
                    let context=handle.pinned_execution_directory(&candidate)?;
                    if context.directory_identity()?!=Some(expected) {return Err("The completed provider directory differs from its original approved directory".into());}
                    Ok(VerifiedDirectory::Completed{identity:expected,context})
                } else {handle.capture_execution_directory(&candidate).map(VerifiedDirectory::Live)}
            }),
            Owner::Terminal(handle) => handle.holder_identity().ok_or_else(|| "The original terminal has no authenticated holder identity; close this pane explicitly or retry after recovery".to_owned()).and_then(|holder| {
                if !holder.holder.is_current() { return Err("The original terminal holder is no longer live; recover or close this pane before retrying Locate".into()); }
                DirectoryProof::capture_owned_process(&holder.root, &candidate, approved).map(VerifiedDirectory::Live)
            }),
            Owner::Unavailable => Err("The original provider is not attached to this daemon. Recover its exact holder, or close this pane and start a new session in the located folder".into()),
        }}.and_then(|proof| {
            if approved.is_some_and(|identity| proof.evidence().identity() != identity) {
                Err("The provider directory differs from its original approved directory; close it before starting in a replacement folder".into())
            } else { Ok(proof) }
        });
        let movement = SessionMove {
            source,
            repository: relocation.destination.clone(),
            candidate,
            proof: proof
                .as_ref()
                .map(VerifiedDirectory::evidence)
                .map_err(Clone::clone),
        };
        prepared.push(Prepared {
            movement,
            owner,
            proof: proof.ok(),
        });
    }
    drop(transitions);
    Ok(PreparedRelocation{edit,prepared,agent_moves,repair,chat_gates,model_committed:false,actor_cursor:0})
}

pub(super) struct PreparedRelocation {
    edit: ModelEdit,
    prepared: Vec<Prepared>,
    agent_moves: Vec<super::model::AgentMove>,
    repair: crate::project_recovery::git_repair::RepairPlan,
    chat_gates:Vec<std::sync::Arc<std::sync::Mutex<()>>>,
    model_committed: bool,
    actor_cursor: usize,
}
impl PreparedRelocation {
    pub(super) fn retained_bytes(&self)->usize {self.repair.retained_bytes()}
    pub(super) fn retained_directories(&self)->usize {self.repair.retained_directories()+self.prepared.len()+self.agent_moves.len()}
    pub(super) fn advance(&mut self,host:&SharedHost,origin:ClientId,authority:&super::project_removal::custody::CommitAuthority<'_>)->Result<WorkspaceModel,ProtocolError> {
        let registry=super::agent_execution::registry(host).map_err(error)?;
        let _transitions=self.chat_gates.iter().map(|gate|match gate.try_lock() {
            Ok(guard)=>Ok(guard),Err(std::sync::TryLockError::Poisoned(poison))=>Ok(poison.into_inner()),
            Err(std::sync::TryLockError::WouldBlock)=>Err(error("The original conversation has a lifecycle transition in progress; its prepared Locate operation was retained")),
        }).collect::<Result<Vec<_>,ProtocolError>>()?;
        if authority.model_is_committed(host)? {self.model_committed=true;}
        {
        let gate=super::client::control_gate(host);
        let _admitted=gate.read().unwrap_or_else(|p|p.into_inner());
        super::client::ensure_control_open(host)?;
        if !self.model_committed {
            authority.before_repair(host)?;
            self.repair.apply_remaining().map_err(error)?;
            authority.pending_commit(host)?;
        }
        #[cfg(test)] if !self.model_committed {authority.before_final_commit(host);}
        self.repair.require_current().map_err(error)?;
        if !self.model_committed {
            for item in &self.prepared {
                match &item.proof {
                    Some(VerifiedDirectory::Live(proof))=>proof.validate_current().map_err(error)?,
                    Some(VerifiedDirectory::Completed{context,identity})=> {
                        context.verify_directory(&item.movement.candidate).map_err(error)?;
                        if context.directory_identity().map_err(error)?!=Some(*identity) {return Err(error("The pending completed provider lost its original retained directory pin"));}
                    }
                    None=>{},
                }
                if let (Some(VerifiedDirectory::Live(proof)),Owner::Chat{handle,..})=(&item.proof,&item.owner) {
                    handle.refresh_retained_state().map_err(error)?;
                    if handle.provider_process().as_ref()!=Some(&proof.process) {
                        return Err(error("The prepared native backend changed; its successor and pending operation were preserved"));
                    }
                }
            }
        }
        } // Release shared effect admission before nonblocking final writer.
    if !self.model_committed {
        let gate=super::client::control_gate(host);
        let _exclusive=super::client::try_scoped_control_write(&gate)?;
        super::client::ensure_control_open(host)?;
        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
        for item in &self.prepared {
            let id = item.movement.source.id();
            let unchanged = match (&item.owner, &item.movement.source) {
                (Owner::Chat{handle,instance}, HostedSession::Chat(source)) => {
                    state.chats.get(&id).is_some_and(|record| {
                        record.execution_instance==*instance && record.handle.same_actor(handle)
                            && match &item.proof {Some(VerifiedDirectory::Live(proof))=>handle.provider_process().as_ref()==Some(&proof.process),Some(VerifiedDirectory::Completed{..})=>handle.is_completed(),None=>true}
                            && HostedSession::Chat(record.session.clone())
                                .same_process(&HostedSession::Chat(source.clone()))
                    })
                }
                (Owner::Terminal(handle), HostedSession::Terminal(source)) => {
                    state
                        .terminals
                        .get(&id)
                        .is_some_and(|record| record.handle.same_worker(handle))
                        && state.descriptor(0, id).is_ok_and(|descriptor| {
                            HostedSession::Terminal(descriptor.session)
                                .same_process(&HostedSession::Terminal(source.clone()))
                        })
                }
                (Owner::Unavailable, _) => {
                    !state.chats.contains_key(&id) && !state.terminals.contains_key(&id)
                }
                _ => false,
            };
            if !unchanged {
                return Err(error(
                    "A hosted session changed while Locate verified its directory; retry without closing or replacing it",
                ));
            }
        }
        let moves = self.prepared
            .iter()
            .map(|item| item.movement.clone())
            .collect::<Vec<_>>();
        state.apply_project_operation(origin,&self.edit,&moves,&self.agent_moves,authority)?;
        self.model_committed=true;
    }
    // The scoped reservation excludes related edits/starts while actor-local
    // operational paths catch up; unrelated input stays responsive. Keep failed
    // actor moves explicitly blocked.
    while let Some(item)=self.prepared.get_mut(self.actor_cursor) {
        let gate=super::client::control_gate(host);
        let _admitted=gate.read().unwrap_or_else(|p|p.into_inner());
        super::client::ensure_control_open(host)?;
        if matches!(&item.proof,Some(VerifiedDirectory::Live(_)))
            && let Owner::Chat{handle,..} = &item.owner
        {
            if let Err(reason) = handle.relocate_execution_directory(&item.movement.candidate) {
                registry
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .refuse_session_location(
                        &item.movement.source,
                        &item.movement.candidate,
                        &reason,
                    )
                    .map_err(error)?;
            }
        }
        self.actor_cursor+=1;
    }
    let state = host.lock().unwrap_or_else(|p| p.into_inner());
    let model = state.read_model()?;
    state.announce_model_edit(origin, model.sequence, &self.edit);
    Ok(model)
}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        agent::Provider,
        chat::{
            self,
            protocol::{Adapter, ProcessSpec, Step, TransportSpec},
            runtime,
        },
        host::{
            chat::Record,
            model::Registry,
            state::{HostLock, HostState},
        },
        model::{
            CardId, HostIncarnation, HostSessionId, HostSessionProof, LaunchRecipe, ProjectKind,
        },
        terminal_ipc::{ClientRequest, ServerResponse},
        workspace_model::views::Pane,
    };
    use std::{
        path::PathBuf,
        sync::{Arc, Mutex},
    };

    fn fixture_request(host:&SharedHost,client:ClientId,request:ClientRequest)->Result<(ServerResponse,Vec<crate::terminal_ipc::ServerMessage>,bool),ProtocolError> {
        let request=match request {
            ClientRequest::EditWorkspaceModel{edit:ModelEdit::RelocateProject{project,relocation}}=>ClientRequest::ProjectOperation{request:crate::project_recovery::ProjectOperationRequest{operation:uuid::Uuid::new_v4(),project,action:crate::project_recovery::ProjectOperationAction::Locate{relocation}}},
            request=>request,
        };
        let (response,messages,close)=super::super::client::process_request(host,client,request)?;
        match response {
            ServerResponse::ProjectOperation{outcome:crate::project_recovery::ProjectOperationOutcome::Committed{model,..}}=>Ok((ServerResponse::WorkspaceModel{model},messages,close)),
            ServerResponse::ProjectOperation{outcome:crate::project_recovery::ProjectOperationOutcome::Pending{reason,..}}=>Err(error(reason)),
            ServerResponse::ProjectOperation{outcome:crate::project_recovery::ProjectOperationOutcome::Aborted{reason,..}}=>Err(error(reason)),
            response=>Ok((response,messages,close)),
        }
    }
    struct PathAware {
        directory: PathBuf,
        calls: Arc<Mutex<Vec<PathBuf>>>,
        pause: Option<(std::sync::mpsc::SyncSender<()>,std::sync::mpsc::Receiver<()>)>,
    }
    impl Adapter for PathAware {
        fn initialize(&mut self) -> Result<Step, String> {
            Ok(Step::default())
        }
        fn receive(&mut self, frame: serde_json::Value) -> Result<Step, String> {
            let mut events = vec![chat::Event::Status { busy: false, detail: "Fixture turn complete".into() }];
            if let Some(id) = frame.get("session").and_then(serde_json::Value::as_str) {
                events.push(chat::Event::Session { id: id.into(), title: None });
            }
            Ok(Step { events, ..Default::default() })
        }
        fn command(&mut self, command: chat::Command) -> Result<Step, String> {
            if matches!(&command,chat::Command::Prompt{text,..} if text=="__fixture_hold") {
                if let Some((entered,release))=self.pause.take() {
                    entered.send(()).map_err(|error|error.to_string())?;
                    release.recv_timeout(Duration::from_secs(10)).map_err(|error|error.to_string())?;
                }
            }
            if matches!(&command, chat::Command::Interrupt) {
                return Ok(Step { events: vec![chat::Event::Status { busy: false, detail: "Fixture interrupted".into() }], ..Default::default() });
            }
            if matches!(&command, chat::Command::Prompt { text, .. } if text == "__fixture_new_thread")
            {
                return Ok(Step {
                    // Conversation confirmation must originate from an actual
                    // provider frame, not a locally prepared command event.
                    writes: vec![serde_json::json!({"session":"44444444-0000-4000-8000-000000000004"})],
                    ..Default::default()
                });
            }
            self.calls.lock().unwrap().push(self.directory.clone());
            Ok(Step { writes: vec![serde_json::json!({"idle":true})], ..Default::default() })
        }
        fn command_for_run(
            &mut self,
            _: crate::model::RunId,
            command: chat::Command,
        ) -> Result<Step, String> {
            self.command(command)
        }
        fn relocate_directory(&mut self, path: &Path) -> Result<(), String> {
            self.directory = path.into();
            Ok(())
        }
    }
    struct Fixture {
        _directory: tempfile::TempDir,
        host: SharedHost,
        handle: Handle,
        source: chat::Session,
        old: PathBuf,
        moved: PathBuf,
        calls: Arc<Mutex<Vec<PathBuf>>>,
        original_context: Option<Arc<chat::protocol::ExecutionContext>>,
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = self.handle.close();
        }
    }
    fn fixture(context_mode: u8) -> Fixture {
        fixture_with_pause(context_mode,None)
    }
    fn fixture_with_pause(context_mode:u8,pause:Option<(std::sync::mpsc::SyncSender<()>,std::sync::mpsc::Receiver<()>)>)->Fixture {
        let directory = tempfile::tempdir().unwrap();
        let old = directory.path().join("original");
        std::fs::create_dir(&old).unwrap();
        let moved = directory.path().join("located");
        // Fixture provider supports native stdin and an observable interactive
        // handoff without invoking any installed or paid agent.
        let program = directory.path().join("omp");
        std::fs::write(&program, "#!/bin/sh\nif [ \"$1\" = --fixture-native ]; then exec /bin/cat; fi\npwd -P > handoff.marker\nexec /bin/cat\n").unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let proof = HostSessionProof {
            host_session_id: HostSessionId::new("session-location-test"),
            boot_id: crate::daemon::read_boot_id().unwrap(),
            host_incarnation: HostIncarnation::new("original"),
        };
        let source = chat::Session {
            id: TerminalId::new(),
            launch: chat::Launch {
                provider: Provider::Omp,
                recipe: LaunchRecipe {
                    program: program.to_string_lossy().into_owned(),
                    arguments: Vec::new(),
                    working_directory: old.clone(),
                    environment: BTreeMap::new(),
                    unset_environment: Default::default(),
                },
                session_id: Some("22222222-0000-4000-8000-000000000001".into()),
            },
            proof: Some(proof.clone()),
            terminal: None,
            native_continuity: Default::default(),
            owned_codex_listener: None,
        };
        let context = match context_mode {
            0 => Some(Arc::new(
                chat::protocol::ExecutionContext::capture(&old).unwrap(),
            )),
            1 => None,
            _ => Some(Arc::new(chat::protocol::ExecutionContext::new(None))),
        };
        let original_context = context.clone();
        let identity = context
            .as_ref()
            .and_then(|context| context.directory_identity().unwrap());
        let spec = ProcessSpec {
            execution: context,
            messages: None,
            unset_environment: Default::default(),
            transport: TransportSpec::JsonLines,
            program: program.to_string_lossy().into_owned(),
            arguments: vec!["--fixture-native".into()],
            working_directory: old.clone(),
            environment: BTreeMap::new(),
        };
        std::fs::write(old.join("relative.txt"), "original provider file").unwrap();
        let mut initial = Step {
            writes: vec![serde_json::json!({"session":source.launch.session_id.as_ref().unwrap()})],
            events: vec![
                chat::Event::Ready(true),
                chat::Event::Session {
                    id: source.launch.session_id.clone().unwrap(),
                    title: None,
                },
                chat::Event::Item(chat::Item {
                    id: "relative-edit".into(),
                    role: chat::Role::Assistant,
                    started_at: None,
                    at: None,
                    content: chat::ItemContent::Changes {
                        changes: vec![chat::FileChange {
                            path: "relative.txt".into(),
                            kind: chat::ChangeKind::Updated,
                            diff: "@@ -1 +1 @@\n-old\n+new\n".into(),
                        }],
                        status: chat::ToolStatus::Succeeded,
                    },
                }),
            ],
            ..Default::default()
        };
        let store = runtime::preflight(&spec, &mut initial).unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let handle = Handle::start(
            spec,
            Box::new(PathAware {
                directory: old.clone(),
                calls: calls.clone(),
                pause,
            }),
            initial,
            store,
            None,
            |_| {},
        )
        .unwrap();
        let mut registry = Registry::at(&directory.path().join("workspace.sqlite3")).unwrap();
        registry
            .apply(&ModelEdit::OpenProject {
                checkout: None,
                primary_checkout: None,
                repository: old.clone(),
                name: "Original".into(),
                kind: ProjectKind::Folder,
            })
            .unwrap();
        registry
            .apply(&ModelEdit::SeedView {
                scope: Scope::Worktree {
                    repository: old.clone(),
                    path: old.clone(),
                },
                panes: vec![Pane {
                    id: CardId::new(),
                    title: "Owned native fixture".into(),
                    user_title: None,
                    pending_terminal_start: false,
                    width_fraction: 0.5,
                    kind: CardKind::Chat(source.clone()),
                    scope: Some(old.clone()),
                }],
                closed: Vec::new(),
            })
            .unwrap();
        if let Some(identity) = identity {
            registry
                .bind_session_location(&HostedSession::Chat(source.clone()), None, &old, identity)
                .unwrap();
        }
        let mut state = HostState::new(proof);
        state.install_model(registry);
        state.chats.insert(
            source.id,
            Record {
                execution_instance: uuid::Uuid::new_v4(),
                console_recipe: None,
                paired_terminal: None,
                session: source.clone(),
                handle: handle.clone(),
                source_process: None,
                subscribers: BTreeMap::from([(1, false)]),
                observations: BTreeMap::new(),
                completion_observed: false,
                returning_terminal: false,
            },
        );
        Fixture {
            _directory: directory,
            host: Arc::new(HostLock::new(state)),
            handle,
            source,
            old,
            moved,
            calls,
            original_context,
        }
    }
    fn locate(fixture: &Fixture) -> WorkspaceModel {
        let request = ClientRequest::EditWorkspaceModel {
            edit: ModelEdit::RelocateProject {
                    project: super::super::agent_execution::registry(&fixture.host).unwrap().lock().unwrap().project_instance(&fixture.old).unwrap(),
                relocation: Relocation {
                    repository: fixture.old.clone(),
                    destination: fixture.moved.clone(),
                    old_root: fixture.old.clone(),
                    new_root: fixture.moved.clone(),
                },
            },
        };
        let response = fixture_request(&fixture.host, 1, request)
            .unwrap()
            .0;
        let ServerResponse::WorkspaceModel { model } = response else {
            panic!("expected verified model")
        };
        model
    }

    #[test]
    fn completed_locate_uses_original_pin_without_a_live_process_or_actor_command() {
        let fixture = fixture(0);
        fixture.handle.close().unwrap();
        assert!(fixture.handle.is_completed());
        let approved = fixture.original_context.as_ref().unwrap().directory_identity().unwrap();
        std::fs::rename(&fixture.old, &fixture.moved).unwrap();
        std::fs::create_dir(&fixture.old).unwrap();
        let model = locate(&fixture);
        assert!(model.session_location_refusals.is_empty());
        assert_eq!(model.projects[0].repository, fixture.moved);
        let registry = super::super::agent_execution::registry(&fixture.host).unwrap();
        let location = registry.lock().unwrap().session_location(&HostedSession::Chat(fixture.source.clone())).unwrap().unwrap();
        assert_eq!(location.directory, fixture.moved);
        assert_eq!(location.approved, approved);
        assert!(location.process.is_none(), "a retained pin never invents a live process witness");
        let (path, context) = verified_chat_directory(&fixture.host, &fixture.source, &fixture.handle).unwrap();
        assert_eq!(path, fixture.moved);
        assert_eq!(context.directory_identity().unwrap(), approved);
        let state = fixture.host.lock().unwrap();
        assert!(state.chats[&fixture.source.id].handle.same_actor(&fixture.handle));
        assert_eq!(state.chats[&fixture.source.id].session, fixture.source);
        assert!(fixture.handle.is_completed());
        assert!(fixture.calls.lock().unwrap().is_empty(), "Locate must not enqueue new provider work");
    }

    #[test]
    fn completed_locate_refuses_replacement_inode_and_missing_original_approval() {
        for context_mode in [0, 1] {
            let fixture = fixture(context_mode);
            fixture.handle.close().unwrap();
            if context_mode == 0 {
                std::fs::rename(&fixture.old, fixture.old.with_extension("retained-original")).unwrap();
            } else {
                std::fs::rename(&fixture.old, &fixture.moved).unwrap();
            }
            if context_mode == 0 { std::fs::create_dir(&fixture.moved).unwrap(); }
            let model = locate(&fixture);
            assert!(!model.session_location_refusals.is_empty());
            assert!(verified_chat_directory(&fixture.host, &fixture.source, &fixture.handle).is_err());
            let state = fixture.host.lock().unwrap();
            assert!(state.chats[&fixture.source.id].handle.same_actor(&fixture.handle));
            assert_eq!(state.chats[&fixture.source.id].session, fixture.source);
            assert!(fixture.handle.is_completed());
        }
    }

    #[test]
    fn completed_pending_locate_rechecks_original_instance_and_candidate_before_commit() {
        for replace_instance in [false,true] {
            let fixture=fixture(0);fixture.handle.close().unwrap();
            std::fs::rename(&fixture.old,&fixture.moved).unwrap();
            let gate=super::super::chat::transition_gate(&fixture.host,fixture.source.id);
            let captured_host=fixture.host.clone();let moved=fixture.moved.clone();let id=fixture.source.id;
            fixture.host.lock().unwrap().project_operation_before_commit=Some(Arc::new(move |_| {
                assert!(matches!(gate.try_lock(),Err(std::sync::TryLockError::WouldBlock)));
                if replace_instance {captured_host.lock().unwrap().chats.get_mut(&id).unwrap().execution_instance=uuid::Uuid::new_v4();}
                else {std::fs::rename(&moved,moved.with_extension("original-pin")).unwrap();std::fs::create_dir(&moved).unwrap();}
            }));
            let request=crate::project_recovery::ProjectOperationRequest{operation:uuid::Uuid::new_v4(),
                project:super::super::agent_execution::registry(&fixture.host).unwrap().lock().unwrap().project_instance(&fixture.old).unwrap(),
                action:crate::project_recovery::ProjectOperationAction::Locate{relocation:Relocation{repository:fixture.old.clone(),destination:fixture.moved.clone(),old_root:fixture.old.clone(),new_root:fixture.moved.clone()}}};
            let (response,_,_)=super::super::client::process_request(&fixture.host,1,ClientRequest::ProjectOperation{request}).unwrap();
            assert!(matches!(response,ServerResponse::ProjectOperation{outcome:crate::project_recovery::ProjectOperationOutcome::Pending{..}}));
            let mut state=fixture.host.lock().unwrap();state.project_operation_before_commit=None;
            assert_eq!(state.read_model().unwrap().projects[0].repository,fixture.old);
            assert_eq!(state.chats[&id].session,fixture.source);
            drop(state);
            assert!(super::super::project_removal::ensure_available(&fixture.host,Some(&fixture.old),None,None).is_err());
        }
    }

    #[test]
    fn locate_keeps_fresh_and_legacy_owned_native_processes_and_immutable_recipes() {
        for context_mode in [0, 1, 2] {
            let fixture = fixture(context_mode);
            let identity = crate::agent_execution::DirectoryIdentity::from_metadata(
                &std::fs::metadata(&fixture.old).unwrap(),
            );
            let Some(chat::FileChangeReply::Files(original_files)) = fixture
                .handle
                .file_query(&chat::Command::LoadFileChanges {
                    request_id: 1,
                    after: None,
                })
                .unwrap()
            else {
                panic!()
            };
            std::fs::rename(&fixture.old, &fixture.moved).unwrap();
            std::fs::create_dir(&fixture.old).unwrap();
            std::fs::write(
                fixture.old.join("relative.txt"),
                "unrelated replacement file",
            )
            .unwrap();
            let model = locate(&fixture);
            assert!(model.session_location_refusals.is_empty());
            assert_eq!(model.projects[0].repository, fixture.moved);
            let state = fixture.host.lock().unwrap();
            let record = &state.chats[&fixture.source.id];
            assert!(record.handle.same_actor(&fixture.handle));
            assert_eq!(record.session, fixture.source);
            drop(state);
            let registry = super::super::agent_execution::registry(&fixture.host).unwrap();
            let saved = registry
                .lock()
                .unwrap()
                .session_location(&HostedSession::Chat(fixture.source.clone()))
                .unwrap()
                .unwrap();
            assert_eq!(saved.directory, fixture.moved);
            assert_eq!(saved.approved, Some(identity));
            if let Some(original_context) = &fixture.original_context {
                assert_eq!(
                    original_context.directory_identity().unwrap(),
                    Some(identity),
                    "legacy empty shared context gains a pin without losing its launch owner"
                );
            }
            fixture
                .handle
                .command_for_run(
                    crate::model::RunId::new(),
                    chat::Command::Prompt {
                        text: "follow-up after Locate".into(),
                        attachments: Vec::new(),
                        mode: chat::PromptMode::Send,
                    },
                )
                .unwrap();
            assert_eq!(*fixture.calls.lock().unwrap(), vec![fixture.moved.clone()]);
            assert!(fixture.old.is_dir());
            let Some(chat::FileChangeReply::Files(located_files)) = fixture
                .handle
                .file_query(&chat::Command::LoadFileChanges {
                    request_id: 2,
                    after: None,
                })
                .unwrap()
            else {
                panic!()
            };
            let resolved = located_files.files[0].resolved_path.as_ref().unwrap();
            assert_eq!(Path::new(resolved), fixture.moved.join("relative.txt"));
            assert_eq!(
                std::fs::read_to_string(resolved).unwrap(),
                "original provider file"
            );
            assert!(located_files.revision > original_files.revision);
            assert_eq!(located_files.applied_files, original_files.applied_files);
        }
    }

    #[test]
    fn stale_issued_launch_or_known_provider_thread_never_binds_a_same_id_actor() {
        for stale_thread in [false, true] {
            let fixture = fixture(1);
            {
                let mut state = fixture.host.lock().unwrap();
                let view = state.read_model().unwrap().views[0].clone();
                let before = view.panes[0].clone();
                let mut pane = before.clone();
                let CardKind::Chat(session) = &mut pane.kind else {
                    unreachable!()
                };
                if stale_thread {
                    session.launch.session_id = Some("33333333-0000-4000-8000-000000000002".into());
                } else {
                    session
                        .launch
                        .recipe
                        .environment
                        .insert("OLD_ISSUED_LAUNCH".into(), "another original".into());
                }
                state
                    .edit_model(
                        1,
                        ModelEdit::ChangeView {
                            scope: view.scope,
                            expected_revision: view.revision,
                            operation: uuid::Uuid::new_v4(),
                            change: crate::workspace_model::views::Change::Update { before, pane },
                        },
                    )
                    .unwrap();
            }
            let before = fixture.host.lock().unwrap().read_model().unwrap();
            std::fs::rename(&fixture.old, &fixture.moved).unwrap();
            let response = fixture_request(
                &fixture.host,
                1,
                ClientRequest::EditWorkspaceModel {
                    edit: ModelEdit::RelocateProject {
                    project: super::super::agent_execution::registry(&fixture.host).unwrap().lock().unwrap().project_instance(&fixture.old).unwrap(),
                        relocation: Relocation {
                            repository: fixture.old.clone(),
                            destination: fixture.moved.clone(),
                            old_root: fixture.old.clone(),
                            new_root: fixture.moved.clone(),
                        },
                    },
                },
            );
            assert!(
                response
                    .unwrap_err()
                    .message
                    .contains("different provider incarnation or issued launch")
            );
            assert_eq!(fixture.host.lock().unwrap().read_model().unwrap(), before);
            assert!(!fixture.handle.is_completed());
        }
    }

    #[test]
    fn owned_actor_thread_change_keeps_directory_authority_but_stale_panes_and_callbacks_do_not() {
        let fixture = fixture(0);
        let instance = fixture.host.lock().unwrap().chats[&fixture.source.id].execution_instance;
        fixture
            .handle
            .command(chat::Command::Prompt {
                text: "__fixture_new_thread".into(),
                attachments: Vec::new(),
                mode: chat::PromptMode::Send,
            })
            .unwrap();
        let new_thread = "44444444-0000-4000-8000-000000000004";
        let deadline = Instant::now() + Duration::from_secs(2);
        while fixture.handle.confirmed_session_id().as_deref() != Some(new_thread) {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
        let registry = super::super::agent_execution::registry(&fixture.host).unwrap();
        super::super::chat::changed_owned(
            &Arc::downgrade(&fixture.host),
            fixture.source.id,
            uuid::Uuid::new_v4(),
            runtime::Update::Presentation,
        );
        assert_eq!(
            fixture.host.lock().unwrap().chats[&fixture.source.id].session,
            fixture.source
        );
        assert!(
            registry
                .lock()
                .unwrap()
                .session_location(&HostedSession::Chat(fixture.source.clone()))
                .unwrap()
                .is_some()
        );
        super::super::chat::changed_owned(
            &Arc::downgrade(&fixture.host),
            fixture.source.id,
            instance,
            runtime::Update::Presentation,
        );
        let current = fixture.host.lock().unwrap().chats[&fixture.source.id]
            .session
            .clone();
        assert_eq!(current.launch.session_id.as_deref(), Some(new_thread));
        assert_eq!(current.launch.recipe, fixture.source.launch.recipe);
        let saved = registry
            .lock()
            .unwrap()
            .session_location(&HostedSession::Chat(current.clone()))
            .unwrap()
            .unwrap();
        assert_eq!(
            saved.approved,
            fixture
                .original_context
                .as_ref()
                .unwrap()
                .directory_identity()
                .unwrap()
        );
        std::fs::rename(&fixture.old, &fixture.moved).unwrap();
        let request = || ClientRequest::EditWorkspaceModel {
            edit: ModelEdit::RelocateProject {
                    project: super::super::agent_execution::registry(&fixture.host).unwrap().lock().unwrap().project_instance(&fixture.old).unwrap(),
                relocation: Relocation {
                    repository: fixture.old.clone(),
                    destination: fixture.moved.clone(),
                    old_root: fixture.old.clone(),
                    new_root: fixture.moved.clone(),
                },
            },
        };
        assert!(
            fixture_request(&fixture.host, 1, request())
                .unwrap_err()
                .message
                .contains("different provider incarnation")
        );
        {
            let mut state = fixture.host.lock().unwrap();
            let view = state.read_model().unwrap().views[0].clone();
            let before = view.panes[0].clone();
            let mut pane = before.clone();
            pane.kind = CardKind::Chat(current);
            state
                .edit_model(
                    1,
                    ModelEdit::ChangeView {
                        scope: view.scope,
                        expected_revision: view.revision,
                        operation: uuid::Uuid::new_v4(),
                        change: crate::workspace_model::views::Change::Update { before, pane },
                    },
                )
                .unwrap();
        }
        assert!(locate(&fixture).session_location_refusals.is_empty());
        assert!(!fixture.handle.is_completed());
    }

    #[test]
    fn failed_thread_binding_write_keeps_actor_output_live_and_authority_closed() {
        let fixture = fixture(0);
        let (instance, delivery) = {
            let mut state = fixture.host.lock().unwrap();
            let delivery = state.register_client(1);
            let record = state.chats.get_mut(&fixture.source.id).unwrap();
            record.subscribers.insert(1, true);
            (record.execution_instance, delivery)
        };
        let database =
            rusqlite::Connection::open(fixture._directory.path().join("workspace.sqlite3"))
                .unwrap();
        database.execute_batch("CREATE TRIGGER reject_session_thread BEFORE UPDATE ON session_locations BEGIN SELECT RAISE(FAIL,'injected directory metadata write failure'); END;").unwrap();
        fixture
            .handle
            .command(chat::Command::Prompt {
                text: "__fixture_new_thread".into(),
                attachments: Vec::new(),
                mode: chat::PromptMode::Send,
            })
            .unwrap();
        let new_thread = "44444444-0000-4000-8000-000000000004";
        let deadline = Instant::now() + Duration::from_secs(2);
        while fixture.handle.confirmed_session_id().as_deref() != Some(new_thread) {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
        super::super::chat::changed_owned(
            &Arc::downgrade(&fixture.host),
            fixture.source.id,
            instance,
            runtime::Update::Presentation,
        );
        delivery.close_after_drain();
        assert!(
            matches!(delivery.pop().map(|message| message.message), Some(crate::terminal_ipc::ServerMessage::ChatChanged { chat_id }) if chat_id == fixture.source.id)
        );
        assert_eq!(
            fixture.host.lock().unwrap().chats[&fixture.source.id].session,
            fixture.source
        );
        assert!(
            fixture
                .host
                .lock()
                .unwrap()
                .directory_thread_retry
                .is_some(),
            "bounded retry after persistence failure"
        );
        let mut current = fixture.source.clone();
        current.launch.session_id = Some(new_thread.into());
        let registry = super::super::agent_execution::registry(&fixture.host).unwrap();
        assert!(
            registry
                .lock()
                .unwrap()
                .session_location(&HostedSession::Chat(current.clone()))
                .is_err()
        );
        std::fs::rename(&fixture.old, &fixture.moved).unwrap();
        let request = ClientRequest::EditWorkspaceModel {
            edit: ModelEdit::RelocateProject {
                    project: super::super::agent_execution::registry(&fixture.host).unwrap().lock().unwrap().project_instance(&fixture.old).unwrap(),
                relocation: Relocation {
                    repository: fixture.old.clone(),
                    destination: fixture.moved.clone(),
                    old_root: fixture.old.clone(),
                    new_root: fixture.moved.clone(),
                },
            },
        };
        assert!(fixture_request(&fixture.host, 1, request).is_err());
        assert!(!fixture.handle.is_completed());
        database
            .execute_batch("DROP TRIGGER reject_session_thread")
            .unwrap();
        let retry = Instant::now() + Duration::from_secs(60);
        fixture.host.lock().unwrap().directory_thread_retry = Some(retry);
        super::super::chat::changed_owned(
            &Arc::downgrade(&fixture.host),
            fixture.source.id,
            instance,
            runtime::Update::Presentation,
        );
        assert_eq!(
            fixture.host.lock().unwrap().directory_thread_retry,
            Some(retry),
            "producer frames cannot hammer a failed database"
        );
        assert_eq!(
            fixture.host.lock().unwrap().chats[&fixture.source.id].session,
            fixture.source
        );
        {
            let mut state = fixture.host.lock().unwrap();
            state.directory_thread_retry = None;
            let view = state.read_model().unwrap().views[0].clone();
            let before = view.panes[0].clone();
            let mut pane = before.clone();
            pane.kind = CardKind::Chat(current.clone());
            state
                .edit_model(
                    1,
                    ModelEdit::ChangeView {
                        scope: view.scope,
                        expected_revision: view.revision,
                        operation: uuid::Uuid::new_v4(),
                        change: crate::workspace_model::views::Change::Update { before, pane },
                    },
                )
                .unwrap();
        }
        // No producer callback follows storage repair: the explicit request
        // must refresh metadata and prove the moved directory on its own.
        assert!(locate(&fixture).session_location_refusals.is_empty());
        assert_eq!(
            fixture.host.lock().unwrap().chats[&fixture.source.id].session,
            current
        );
        assert!(
            registry
                .lock()
                .unwrap()
                .session_location(&HostedSession::Chat(current))
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn closed_legacy_chat_id_cannot_name_a_fresh_same_recipe_actor() {
        let fixture = fixture(1);
        assert!(
            !fixture
                .host
                .lock()
                .unwrap()
                .issued_chat_ids
                .contains(&fixture.source.id)
        );
        fixture_request(
            &fixture.host,
            1,
            ClientRequest::ChatClose {
                chat_id: fixture.source.id,
            },
        )
        .unwrap();
        assert!(
            !fixture
                .host
                .lock()
                .unwrap()
                .chats
                .contains_key(&fixture.source.id)
        );
        let mut replacement = fixture.source.clone();
        replacement.launch.session_id = None;
        let response = fixture_request(
            &fixture.host,
            1,
            ClientRequest::ChatStart {
                session: replacement,
                source: None,
                terminate_source: false,
                execution_environment: Default::default(),
            },
        );
        assert!(
            response
                .unwrap_err()
                .message
                .contains("session ID was retired")
        );
        assert!(
            !fixture
                .host
                .lock()
                .unwrap()
                .chats
                .contains_key(&fixture.source.id)
        );
    }

    #[test]
    fn locate_preserves_an_ordinary_registered_terminal_with_raw_launch_continuity() {
        const CHILD: &str = "CAGENTS_LOCATE_TERMINAL_TEST";
        if std::env::var_os(CHILD).is_none() {
            let directory = tempfile::tempdir().unwrap();
            for name in ["data", "config"] {
                std::fs::create_dir(directory.path().join(name)).unwrap();
            }
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "host::session_locations::tests::locate_preserves_an_ordinary_registered_terminal_with_raw_launch_continuity", "--nocapture"])
                .env_clear().env(CHILD, "1").env("HOME", directory.path())
                .env("XDG_DATA_HOME", directory.path().join("data")).env("XDG_CONFIG_HOME", directory.path().join("config"))
                .env("XDG_RUNTIME_DIR", directory.path()).env("TMPDIR", directory.path()).env("PATH", "/usr/bin:/bin").spawn().unwrap();
            let deadline = Instant::now() + Duration::from_secs(20);
            loop {
                if let Some(status) = child.try_wait().unwrap() {
                    assert!(
                        status.success(),
                        "registered terminal relocation fixture failed"
                    );
                    return;
                }
                if Instant::now() >= deadline {
                    child.kill().unwrap();
                    child.wait().unwrap();
                    panic!("registered terminal relocation fixture timed out");
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        let fixture = fixture(0);
        let paths = crate::daemon::RuntimePaths::discover().unwrap();
        let proof = fixture.host.lock().unwrap().proof().clone();
        let _lease = crate::daemon::HostLease::acquire(&paths, &proof).unwrap();
        let recipe = LaunchRecipe {
            program: "/bin/cat".into(),
            arguments: Vec::new(),
            working_directory: fixture.old.clone(),
            environment: Default::default(),
            unset_environment: Default::default(),
        };
        let terminal = crate::model::TerminalSession {
            id: TerminalId::new(),
            owned_codex_listener: None,
            recovery_origin: None,
            predecessor: None,
            launch: recipe.clone(),
            transcript_path: None,
            resume_hint: None,
            continuity: crate::model::TerminalContinuity::RelaunchAvailable {
                recipe,
                last_exit: None,
            },
        };
        {
            let mut state = fixture.host.lock().unwrap();
            state.enable_holder_recovery();
            state.register_client(1);
        }
        let descriptor = super::super::state::spawn_terminal(
            &fixture.host,
            1,
            terminal.clone(),
            Default::default(),
        )
        .unwrap();
        struct OwnedTerminal(TerminalHandle);
        impl Drop for OwnedTerminal {
            fn drop(&mut self) {
                let _ = self.0.shutdown();
            }
        }
        let owner = OwnedTerminal(
            fixture.host.lock().unwrap().terminals[&terminal.id]
                .handle
                .clone(),
        );
        let holder = owner.0.holder_identity().expect("actual registered holder");
        // A shell can leave its issued launch directory. Its current subdir
        // proves no authority over the whole mapped root; retain it and report
        // the refusal while an independently provable terminal still moves.
        let child = fixture.old.join("child");
        std::fs::create_dir(&child).unwrap();
        let mut changed_cwd = terminal.clone();
        changed_cwd.id = TerminalId::new();
        changed_cwd.launch.program = "/bin/sh".into();
        changed_cwd.launch.arguments = vec!["-c".into(), "cd child && exec cat".into()];
        changed_cwd.continuity = crate::model::TerminalContinuity::RelaunchAvailable {
            recipe: changed_cwd.launch.clone(),
            last_exit: None,
        };
        let changed_descriptor = super::super::state::spawn_terminal(
            &fixture.host,
            1,
            changed_cwd.clone(),
            Default::default(),
        )
        .unwrap();
        let changed_owner = OwnedTerminal(
            fixture.host.lock().unwrap().terminals[&changed_cwd.id]
                .handle
                .clone(),
        );
        let changed_holder = changed_owner.0.holder_identity().unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while std::fs::read_link(format!("/proc/{}/cwd", changed_holder.root.pid)).unwrap() != child
        {
            assert!(
                Instant::now() < deadline,
                "terminal did not enter its child working directory"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        {
            let mut state = fixture.host.lock().unwrap();
            assert!(matches!(
                state.terminals[&terminal.id].session.continuity,
                crate::model::TerminalContinuity::RelaunchAvailable { .. }
            ));
            let view = state.read_model().unwrap().views[0].clone();
            state
                .edit_model(
                    1,
                    ModelEdit::ChangeView {
                        scope: view.scope,
                        expected_revision: view.revision,
                        operation: uuid::Uuid::new_v4(),
                        change: crate::workspace_model::views::Change::Insert {
                            pane: Pane {
                                id: CardId::new(),
                                title: "Registered terminal".into(),
                                user_title: None,
                                pending_terminal_start: false,
                                width_fraction: 0.5,
                                kind: CardKind::Terminal(descriptor.session.clone()),
                                scope: Some(fixture.old.clone()),
                            },
                            before: None,
                        },
                    },
                )
                .unwrap();
            let view = state.read_model().unwrap().views[0].clone();
            state
                .edit_model(
                    1,
                    ModelEdit::ChangeView {
                        scope: view.scope,
                        expected_revision: view.revision,
                        operation: uuid::Uuid::new_v4(),
                        change: crate::workspace_model::views::Change::Insert {
                            pane: Pane {
                                id: CardId::new(),
                                title: "Shell in child directory".into(),
                                user_title: None,
                                pending_terminal_start: false,
                                width_fraction: 0.5,
                                kind: CardKind::Terminal(changed_descriptor.session.clone()),
                                scope: Some(fixture.old.clone()),
                            },
                            before: None,
                        },
                    },
                )
                .unwrap();
        }
        std::fs::rename(&fixture.old, &fixture.moved).unwrap();
        std::fs::create_dir(&fixture.old).unwrap();
        let model = locate(&fixture);
        assert_eq!(model.session_location_refusals.len(), 1);
        assert_eq!(model.session_location_refusals[0].session, changed_cwd.id);
        assert!(
            model.session_location_refusals[0]
                .reason
                .contains("different directory")
        );
        let state = fixture.host.lock().unwrap();
        assert!(state.terminals[&terminal.id].handle.same_worker(&owner.0));
        assert_eq!(
            state.terminals[&terminal.id].session.launch,
            terminal.launch
        );
        assert_eq!(
            state.terminals[&terminal.id]
                .handle
                .holder_identity()
                .unwrap(),
            holder
        );
        assert!(holder.root.is_current());
        assert!(!owner.0.is_completed());
        assert!(!changed_owner.0.is_completed());
        assert!(changed_holder.root.is_current());
        drop(state);
        let registry = super::super::agent_execution::registry(&fixture.host).unwrap();
        assert_eq!(
            registry
                .lock()
                .unwrap()
                .session_location(&HostedSession::Terminal(descriptor.session))
                .unwrap()
                .unwrap()
                .directory,
            fixture.moved
        );
        let response = fixture_request(
            &fixture.host,
            1,
            ClientRequest::ChatReturnTerminal {
                chat_id: fixture.source.id,
            },
        )
        .unwrap()
        .0;
        let ServerResponse::ChatTerminal {
            session: replacement,
            ..
        } = response
        else {
            panic!("expected exact terminal handoff")
        };
        let replacement_owner = OwnedTerminal(
            fixture.host.lock().unwrap().terminals[&replacement.id]
                .handle
                .clone(),
        );
        assert_eq!(replacement.launch.working_directory, fixture.moved);
        assert_eq!(fixture.source.launch.recipe.working_directory, fixture.old);
        let marker = fixture.moved.join("handoff.marker");
        let deadline = Instant::now() + Duration::from_secs(3);
        while !marker.exists() {
            assert!(
                Instant::now() < deadline,
                "terminal handoff did not run in its pinned directory"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            std::fs::read_to_string(marker).unwrap().trim(),
            fixture.moved.to_str().unwrap()
        );
        assert!(!fixture.old.join("handoff.marker").exists());
        assert!(!replacement_owner.0.is_completed());
    }

    #[test]
    fn locate_replacement_folder_retains_provider_and_returns_actionable_refusal() {
        let fixture = fixture(0);
        let retained = fixture._directory.path().join("actual-provider-folder");
        std::fs::rename(&fixture.old, &retained).unwrap();
        std::fs::create_dir(&fixture.old).unwrap();
        std::fs::create_dir(&fixture.moved).unwrap();
        let model = locate(&fixture);
        assert_eq!(model.projects[0].repository, fixture.moved);
        assert_eq!(model.session_location_refusals.len(), 1);
        assert_eq!(
            model.session_location_refusals[0].session,
            fixture.source.id
        );
        assert!(
            model.session_location_refusals[0]
                .reason
                .contains("different directory")
        );
        assert!(
            !fixture.handle.is_completed(),
            "Locate never stops the original provider"
        );
        assert!(
            fixture
                .handle
                .command(chat::Command::Prompt {
                    text: "must not enter replacement".into(),
                    attachments: Vec::new(),
                    mode: chat::PromptMode::Send
                })
                .is_err()
        );
        assert!(fixture.calls.lock().unwrap().is_empty());
        assert_eq!(
            fixture.host.lock().unwrap().chats[&fixture.source.id].session,
            fixture.source
        );
        assert!(fixture.moved.is_dir());
        // Restoring the positively identified original directory at the same
        // located spelling allows Verify again without another logical move.
        std::fs::rename(
            &fixture.moved,
            fixture._directory.path().join("unapproved-replacement"),
        )
        .unwrap();
        std::fs::rename(&retained, &fixture.moved).unwrap();
        let response = fixture_request(
            &fixture.host,
            1,
            ClientRequest::EditWorkspaceModel {
                edit: ModelEdit::RelocateProject {
                    project: super::super::agent_execution::registry(&fixture.host).unwrap().lock().unwrap().project_instance(&fixture.moved).unwrap(),
                    relocation: Relocation {
                        repository: fixture.moved.clone(),
                        destination: fixture.moved.clone(),
                        old_root: fixture.moved.clone(),
                        new_root: fixture.moved.clone(),
                    },
                },
            },
        )
        .unwrap()
        .0;
        let ServerResponse::WorkspaceModel { model: verified } = response else {
            panic!()
        };
        assert!(verified.session_location_refusals.is_empty());
        assert!(verified.sequence > model.sequence);
        fixture
            .handle
            .command_for_run(
                crate::model::RunId::new(),
                chat::Command::Prompt {
                    text: "follow-up after positive retry".into(),
                    attachments: Vec::new(),
                    mode: chat::PromptMode::Send,
                },
            )
            .unwrap();
        assert_eq!(*fixture.calls.lock().unwrap(), vec![fixture.moved.clone()]);
    }
    #[test]
    fn locate_provider_proof_keeps_unrelated_admission_responsive_and_releases_scope() {
        let (entered_tx,entered_rx)=std::sync::mpsc::sync_channel(1);
        let (release_tx,release_rx)=std::sync::mpsc::sync_channel(1);
        let fixture=fixture_with_pause(1,Some((entered_tx,release_rx)));
        let expected=super::super::agent_execution::registry(&fixture.host).unwrap().lock().unwrap().project_instance(&fixture.old).unwrap();
        let handle=fixture.handle.clone();
        let paused=std::thread::spawn(move||handle.command(chat::Command::Prompt{text:"__fixture_hold".into(),attachments:Vec::new(),mode:chat::PromptMode::Send}));
        entered_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        struct Release(Option<std::sync::mpsc::SyncSender<()>>);
        impl Drop for Release { fn drop(&mut self){if let Some(sender)=self.0.take(){let _=sender.send(());}} }
        let release=Release(Some(release_tx));
        std::fs::rename(&fixture.old,&fixture.moved).unwrap();
        let request=ClientRequest::EditWorkspaceModel{edit:ModelEdit::RelocateProject{project:expected,relocation:Relocation{repository:fixture.old.clone(),destination:fixture.moved.clone(),old_root:fixture.old.clone(),new_root:fixture.moved.clone()}}};
        let host=fixture.host.clone();
        let locating=std::thread::spawn(move||fixture_request(&host,1,request));
        let deadline=Instant::now()+Duration::from_secs(3);
        loop {
            let reserved={let state=fixture.host.lock().unwrap();let entries=state.project_removals.lock().unwrap();!entries.is_empty()};
            if reserved {break;}
            assert!(Instant::now()<deadline,"Locate must install its project reservation before provider proof");
            std::thread::yield_now();
        }
        let (done_tx,done_rx)=std::sync::mpsc::sync_channel(1);
        let host=fixture.host.clone();
        let unrelated=std::thread::spawn(move|| { let result=fixture_request(&host,2,ClientRequest::List);done_tx.send(result.is_ok()).unwrap(); });
        assert!(done_rx.recv_timeout(Duration::from_millis(500)).unwrap(),"Unrelated List must not wait for the provider's proof");
        unrelated.join().unwrap();
        assert!(super::super::project_removal::ensure_available(&fixture.host,Some(&fixture.old),None,None).is_err());
        assert!(super::super::project_removal::ensure_available(&fixture.host,Some(&fixture.moved),None,None).is_err());
        drop(release);
        paused.join().unwrap().unwrap();
        locating.join().unwrap().unwrap();
        assert!(fixture.host.lock().unwrap().project_removals.lock().unwrap().is_empty());
    }

    #[test]
    fn stale_locate_request_preserves_live_provider_and_reopened_project_before_proof_or_repair() {
        let fixture=fixture(1);
        let registry=super::super::agent_execution::registry(&fixture.host).unwrap();
        let expected=registry.lock().unwrap().project_instance(&fixture.old).unwrap();
        {let mut registry=registry.lock().unwrap();registry.apply(&ModelEdit::CloseProject{repository:fixture.old.clone()}).unwrap();registry.apply(&ModelEdit::ReopenProject{repository:fixture.old.clone(),checkout:None,primary_checkout:None,name:"Replacement".into(),kind:ProjectKind::Folder}).unwrap();}
        let before=registry.lock().unwrap().model().unwrap();
        let request=ClientRequest::EditWorkspaceModel{edit:ModelEdit::RelocateProject{project:expected,relocation:Relocation{repository:fixture.old.clone(),destination:fixture.moved.clone(),old_root:fixture.old.clone(),new_root:fixture.moved.clone()}}};
        assert!(fixture_request(&fixture.host,1,request).unwrap_err().message.contains("replaced"));
        assert_eq!(registry.lock().unwrap().model().unwrap(),before);
        assert!(!fixture.handle.is_completed());
        assert!(fixture.calls.lock().unwrap().is_empty());
        assert!(!fixture.moved.exists());
        assert!(fixture.host.lock().unwrap().project_removals.lock().unwrap().is_empty());
    }

}
