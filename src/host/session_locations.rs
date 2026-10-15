//! Prepare process evidence outside HostState, then atomically relocate logical
//! ownership and its exact sessions. Caller holds exclusive control admission.
use super::{model::SessionMove, state::{ClientId, SharedHost}};
use crate::{agent_execution::HostedSession, chat::runtime::{DirectoryProof, Handle}, model::{CardKind, TerminalId}, project_recovery::Relocation, terminal::TerminalHandle, terminal_ipc::{ProtocolError, ProtocolErrorCode}, workspace_model::{ModelEdit, WorkspaceModel, views::Scope}};
use std::{collections::BTreeMap, path::Path, time::{Duration, Instant}};

#[derive(Clone)]
enum Owner { Chat(Handle), Terminal(TerminalHandle), Unavailable }
struct Prepared { movement: SessionMove, owner: Owner, proof: Option<DirectoryProof> }

fn error(message: impl Into<String>) -> ProtocolError {
    ProtocolError::new(ProtocolErrorCode::InvalidRequest, message, false)
}
fn issued_directory(source: &HostedSession) -> &Path {
    match source { HostedSession::Chat(source) => &source.launch.recipe.working_directory, HostedSession::Terminal(source) => &source.launch.working_directory }
}
fn owned(scope: &Scope, model: &WorkspaceModel, relocation: &Relocation) -> bool {
    match scope {
        Scope::Worktree { repository, .. } => *repository == relocation.repository,
        Scope::Channel(id) => model.channel(*id).is_some_and(|channel| channel.repository == relocation.repository),
    }
}

/// Capture before any destructive handoff or account switch. The returned
/// context owns an independent FD pin, never the source actor's cleanup owner.
pub(super) fn verified_chat_directory(host: &SharedHost, source: &crate::chat::Session, expected: &Handle) -> Result<(std::path::PathBuf, std::sync::Arc<crate::chat::protocol::ExecutionContext>), String> {
    use crate::chat::protocol::ExecutionContext;
    use std::sync::Arc;
    let current = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        let record = state.chats.get(&source.id).ok_or("The original native provider is no longer hosted")?;
        if !record.handle.same_actor(expected) { return Err("The native provider changed before its directory could be captured".into()); }
        let mut current = record.session.clone();
        current.launch.session_id = record.handle.confirmed_session_id().or(current.launch.session_id);
        current
    };
    let registry = super::agent_execution::registry(host)?;
    let binding = {
        let registry = registry.lock().unwrap_or_else(|p| p.into_inner());
        if !registry.authenticated_session_alias(&HostedSession::Chat(source.clone()), &HostedSession::Chat(current.clone()))? {
            return Err("The pane does not name this original provider incarnation; refresh its ownership before retrying".into());
        }
        registry.session_location(&HostedSession::Chat(current.clone()))?
    };
    if let Some(reason) = binding.as_ref().and_then(|binding| binding.refusal.as_ref()) { return Err(reason.clone()); }
    let directory = binding.as_ref().map(|binding| binding.directory.clone()).unwrap_or_else(|| current.launch.recipe.working_directory.clone());
    let context = if expected.is_completed() {
        if binding.as_ref().and_then(|binding| binding.approved).is_none() { return Err("The completed source has no verified operational directory binding; create a new session explicitly".into()); }
        expected.pinned_execution_directory(&directory)?
    } else {
        let proof = expected.capture_execution_directory(&directory)?;
        Arc::new(ExecutionContext::new(Some(proof.directory)))
    };
    if let Some(approved) = binding.as_ref().and_then(|binding| binding.approved)
        && context.directory_identity()? != Some(approved) { return Err("The original provider directory no longer matches its approved identity".into()); }
    let state = host.lock().unwrap_or_else(|p| p.into_inner());
    if !state.chats.get(&source.id).is_some_and(|record| record.handle.same_actor(expected) && HostedSession::Chat(record.session.clone()).same_process(&HostedSession::Chat(current))) {
        return Err("The native provider changed while its directory was being captured; retry without stopping it".into());
    }
    Ok((directory, context))
}

pub(super) fn relocate(host: &SharedHost, origin: ClientId, relocation: Relocation) -> Result<WorkspaceModel, ProtocolError> {
    let edit = ModelEdit::RelocateProject { relocation }.canonicalized().map_err(error)?;
    let ModelEdit::RelocateProject { relocation } = &edit else { unreachable!() };
    let registry = super::agent_execution::registry(host).map_err(error)?;
    let sources = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        let model = state.read_model()?;
        let mut sources = BTreeMap::<TerminalId, HostedSession>::new();
        for view in &model.views {
            if !owned(&view.scope, &model, relocation) { continue; }
            for pane in &view.panes {
                match &pane.kind {
                    CardKind::Chat(session) if state.chats.contains_key(&session.id) || !session.launch.needs_read_only_history() => { sources.insert(session.id, HostedSession::Chat(session.clone())); },
                    CardKind::Terminal(session) if session.continuity.live_proof().is_some() => { sources.entry(session.id).or_insert_with(|| HostedSession::Terminal(session.clone())); },
                    _ => {},
                }
            }
        }
        for agent in &model.agents {
            let repository = model.agent_locations.iter().find(|location| location.agent == agent.id)
                .map(|location| &location.repository).unwrap_or(&agent.spec.target.repository);
            if *repository == relocation.repository && let Some(source) = &agent.session {
                sources.insert(source.id(), source.clone());
            }
        }
        // A numeric ID locates a possible owner; exact source or authenticated
        // retained lineage must still prove the association outside this lock.
        sources.into_iter().map(|(id, source)| {
            let owner = if let HostedSession::Chat(_) = &source
                && let Some(record) = state.chats.get(&id) {
                let mut current = record.session.clone();
                current.launch.session_id = record.handle.confirmed_session_id().or(current.launch.session_id);
                (source, HostedSession::Chat(current), Owner::Chat(record.handle.clone()))
            } else if let HostedSession::Terminal(_) = &source
                && let Some(record) = state.terminals.get(&id) {
                // Raw records retain launch continuity. Only HostState projects
                // the authenticated current daemon proof for a live worker.
                state.descriptor(0, id).map(|descriptor| (source, HostedSession::Terminal(descriptor.session), Owner::Terminal(record.handle.clone())))?
            } else { (source.clone(), source, Owner::Unavailable) };
            Ok(owner)
        }).collect::<Result<Vec<_>,ProtocolError>>()?
    };
    let agent_inputs = registry.lock().unwrap_or_else(|p| p.into_inner()).agent_relocation_inputs(relocation).map_err(error)?;
    let agent_moves = agent_inputs.into_iter().map(|input| input.verify()).collect::<Vec<_>>();
    let mut prepared = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    for (cached, source, owner) in sources {
        if !matches!(owner, Owner::Unavailable)
            && !registry.lock().unwrap_or_else(|p| p.into_inner()).authenticated_session_alias(&cached, &source).map_err(error)? {
            return Err(error("A saved pane names a different provider incarnation or issued launch. Refresh this pane from its original owner before locating its project; no session was moved"));
        }
        let previous = registry.lock().unwrap_or_else(|p| p.into_inner()).session_location(&source).map_err(error)?;
        let directory = previous.as_ref().map(|location| location.blocked_directory.as_deref().unwrap_or(&location.directory)).unwrap_or_else(|| issued_directory(&source));
        let candidate = relocation.mapped(directory);
        let approved = previous.as_ref().and_then(|location| location.approved);
        let proof = if Instant::now() >= deadline {
            Err("Locate reached its provider verification time limit; retry after these providers are responsive, or close the affected panes explicitly".into())
        } else { match &owner {
            Owner::Chat(handle) => handle.capture_execution_directory(&candidate),
            Owner::Terminal(handle) => handle.holder_identity().ok_or_else(|| "The original terminal has no authenticated holder identity; close this pane explicitly or retry after recovery".to_owned()).and_then(|holder| {
                if !holder.holder.is_current() { return Err("The original terminal holder is no longer live; recover or close this pane before retrying Locate".into()); }
                DirectoryProof::capture_owned_process(&holder.root, &candidate, approved)
            }),
            Owner::Unavailable => Err("The original provider is not attached to this daemon. Recover its exact holder, or close this pane and start a new session in the located folder".into()),
        }}.and_then(|proof| {
            if approved.is_some_and(|identity| proof.identity != identity) {
                Err("The provider directory differs from its original approved directory; close it before starting in a replacement folder".into())
            } else { Ok(proof) }
        });
        let movement = SessionMove { source, repository: relocation.destination.clone(), candidate, proof: proof.as_ref().map(|proof| (proof.identity, proof.process.clone())).map_err(Clone::clone) };
        prepared.push(Prepared { movement, owner, proof: proof.ok() });
    }
    {
        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
        for item in &prepared {
            let id = item.movement.source.id();
            let unchanged = match (&item.owner, &item.movement.source) {
                (Owner::Chat(handle), HostedSession::Chat(source)) => state.chats.get(&id).is_some_and(|record| record.handle.same_actor(handle) && HostedSession::Chat(record.session.clone()).same_process(&HostedSession::Chat(source.clone()))),
                (Owner::Terminal(handle), HostedSession::Terminal(source)) => state.terminals.get(&id).is_some_and(|record| record.handle.same_worker(handle)) && state.descriptor(0,id).is_ok_and(|descriptor| HostedSession::Terminal(descriptor.session).same_process(&HostedSession::Terminal(source.clone()))),
                (Owner::Unavailable, _) => !state.chats.contains_key(&id) && !state.terminals.contains_key(&id),
                _ => false,
            };
            if !unchanged { return Err(error("A hosted session changed while Locate verified its directory; retry without closing or replacing it")); }
        }
        let moves = prepared.iter().map(|item| item.movement.clone()).collect::<Vec<_>>();
        state.apply_session_relocation(&edit, &moves, &agent_moves)?;
    }
    // The exclusive gate excludes further edits/starts while actor-local
    // operational paths catch up. Keep any failed actor move explicitly blocked.
    for item in &mut prepared {
        if item.proof.is_some() && let Owner::Chat(handle) = &item.owner {
            if let Err(reason) = handle.relocate_execution_directory(&item.movement.candidate) {
                registry.lock().unwrap_or_else(|p| p.into_inner()).refuse_session_location(&item.movement.source, &item.movement.candidate, &reason).map_err(error)?;
            }
        }
    }
    let state = host.lock().unwrap_or_else(|p| p.into_inner());
    let model = state.read_model()?;
    state.announce_model_edit(origin, model.sequence, &edit);
    Ok(model)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{agent::Provider, chat::{self, protocol::{Adapter, ProcessSpec, Step, TransportSpec}, runtime}, host::{chat::Record, model::Registry, state::{HostLock, HostState}}, model::{CardId, HostIncarnation, HostSessionId, HostSessionProof, LaunchRecipe, ProjectKind}, terminal_ipc::{ClientRequest, ServerResponse}, workspace_model::views::Pane};
    use std::{path::PathBuf, sync::{Arc, Mutex}};

    struct PathAware { directory: PathBuf, calls: Arc<Mutex<Vec<PathBuf>>> }
    impl Adapter for PathAware {
        fn initialize(&mut self) -> Result<Step,String> { Ok(Step::default()) }
        fn receive(&mut self, _: serde_json::Value) -> Result<Step,String> { Ok(Step::default()) }
        fn command(&mut self, _: chat::Command) -> Result<Step,String> { self.calls.lock().unwrap().push(self.directory.clone()); Ok(Step::default()) }
        fn command_for_run(&mut self, _: crate::model::RunId, command: chat::Command) -> Result<Step,String> { self.command(command) }
        fn relocate_directory(&mut self, path: &Path) -> Result<(),String> { self.directory = path.into(); Ok(()) }
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
    impl Drop for Fixture { fn drop(&mut self) { let _ = self.handle.close(); } }
    fn fixture(context_mode: u8) -> Fixture {
        let directory = tempfile::tempdir().unwrap();
        let old = directory.path().join("original"); std::fs::create_dir(&old).unwrap();
        let moved = directory.path().join("located");
        let proof = HostSessionProof { host_session_id: HostSessionId::new("session-location-test"), boot_id: crate::daemon::read_boot_id().unwrap(), host_incarnation: HostIncarnation::new("original") };
        let source = chat::Session { id: TerminalId::new(), launch: chat::Launch { provider: Provider::Omp, recipe: LaunchRecipe { program: "/bin/cat".into(), arguments: Vec::new(), working_directory: old.clone(), environment: BTreeMap::new(), unset_environment: Default::default() }, session_id: Some("22222222-0000-4000-8000-000000000001".into()) }, proof: Some(proof.clone()), terminal: None, native_continuity: Default::default(), owned_codex_listener: None };
        let context = match context_mode {
            0 => Some(Arc::new(chat::protocol::ExecutionContext::capture(&old).unwrap())),
            1 => None,
            _ => Some(Arc::new(chat::protocol::ExecutionContext::new(None))),
        };
        let original_context = context.clone();
        let identity = context.as_ref().and_then(|context| context.directory_identity().unwrap());
        let spec = ProcessSpec { execution: context, messages: None, unset_environment: Default::default(), transport: TransportSpec::JsonLines, program: "/bin/cat".into(), arguments: Vec::new(), working_directory: old.clone(), environment: BTreeMap::new() };
        std::fs::write(old.join("relative.txt"), "original provider file").unwrap();
        let mut initial = Step { events: vec![chat::Event::Ready(true), chat::Event::Session { id: source.launch.session_id.clone().unwrap(), title: None }, chat::Event::Item(chat::Item { id: "relative-edit".into(), role: chat::Role::Assistant, started_at: None, at: None, content: chat::ItemContent::Changes { changes: vec![chat::FileChange { path: "relative.txt".into(), kind: chat::ChangeKind::Updated, diff: "@@ -1 +1 @@\n-old\n+new\n".into() }], status: chat::ToolStatus::Succeeded } })], ..Default::default() };
        let store = runtime::preflight(&spec, &mut initial).unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let handle = Handle::start(spec, Box::new(PathAware { directory: old.clone(), calls: calls.clone() }), initial, store, None, |_| {}).unwrap();
        let mut registry = Registry::at(&directory.path().join("workspace.sqlite3")).unwrap();
        registry.apply(&ModelEdit::OpenProject { checkout: None, primary_checkout: None, repository: old.clone(), name: "Original".into(), kind: ProjectKind::Folder }).unwrap();
        registry.apply(&ModelEdit::SeedView { scope: Scope::Worktree { repository: old.clone(), path: old.clone() }, panes: vec![Pane { id: CardId::new(), title: "Owned native fixture".into(), user_title: None, pending_terminal_start: false, width_fraction: 0.5, kind: CardKind::Chat(source.clone()), scope: Some(old.clone()) }], closed: Vec::new() }).unwrap();
        if let Some(identity) = identity { registry.bind_session_location(&HostedSession::Chat(source.clone()), None, &old, identity).unwrap(); }
        let mut state = HostState::new(proof);
        state.install_model(registry);
        state.chats.insert(source.id, Record { execution_instance: uuid::Uuid::new_v4(), console_recipe: None, paired_terminal: None, session: source.clone(), handle: handle.clone(), source_process: None, subscribers: BTreeMap::from([(1, false)]), observations: BTreeMap::new(), completion_observed: false, returning_terminal: false });
        Fixture { _directory: directory, host: Arc::new(HostLock::new(state)), handle, source, old, moved, calls, original_context }
    }
    fn locate(fixture: &Fixture) -> WorkspaceModel {
        let request = ClientRequest::EditWorkspaceModel { edit: ModelEdit::RelocateProject { relocation: Relocation { repository: fixture.old.clone(), destination: fixture.moved.clone(), old_root: fixture.old.clone(), new_root: fixture.moved.clone() } } };
        let response = super::super::client::process_request(&fixture.host, 1, request).unwrap().0;
        let ServerResponse::WorkspaceModel { model } = response else { panic!("expected verified model") }; model
    }

    #[test]
    fn locate_keeps_fresh_and_legacy_owned_native_processes_and_immutable_recipes() {
        for context_mode in [0, 1, 2] {
            let fixture = fixture(context_mode);
            let identity = crate::agent_execution::DirectoryIdentity::from_metadata(&std::fs::metadata(&fixture.old).unwrap());
            let Some(chat::FileChangeReply::Files(original_files)) = fixture.handle.file_query(&chat::Command::LoadFileChanges { request_id: 1, after: None }).unwrap() else { panic!() };
            std::fs::rename(&fixture.old, &fixture.moved).unwrap();
            std::fs::create_dir(&fixture.old).unwrap();
            std::fs::write(fixture.old.join("relative.txt"), "unrelated replacement file").unwrap();
            let model = locate(&fixture);
            assert!(model.session_location_refusals.is_empty());
            assert_eq!(model.projects[0].repository, fixture.moved);
            let state = fixture.host.lock().unwrap();
            let record = &state.chats[&fixture.source.id];
            assert!(record.handle.same_actor(&fixture.handle));
            assert_eq!(record.session, fixture.source);
            drop(state);
            let registry = super::super::agent_execution::registry(&fixture.host).unwrap();
            let saved = registry.lock().unwrap().session_location(&HostedSession::Chat(fixture.source.clone())).unwrap().unwrap();
            assert_eq!(saved.directory, fixture.moved);
            assert_eq!(saved.approved, Some(identity));
            if let Some(original_context) = &fixture.original_context {
                assert_eq!(original_context.directory_identity().unwrap(), Some(identity), "legacy empty shared context gains a pin without losing its launch owner");
            }
            fixture.handle.command_for_run(crate::model::RunId::new(), chat::Command::Prompt { text: "follow-up after Locate".into(), attachments: Vec::new(), mode: chat::PromptMode::Send }).unwrap();
            assert_eq!(*fixture.calls.lock().unwrap(), vec![fixture.moved.clone()]);
            assert!(fixture.old.is_dir());
            let Some(chat::FileChangeReply::Files(located_files)) = fixture.handle.file_query(&chat::Command::LoadFileChanges { request_id: 2, after: None }).unwrap() else { panic!() };
            let resolved = located_files.files[0].resolved_path.as_ref().unwrap();
            assert_eq!(Path::new(resolved), fixture.moved.join("relative.txt"));
            assert_eq!(std::fs::read_to_string(resolved).unwrap(), "original provider file");
            assert!(located_files.revision > original_files.revision);
            assert_eq!(located_files.applied_files, original_files.applied_files);
        }
    }

    #[test]
    fn stale_issued_launch_or_known_provider_thread_never_binds_a_same_id_actor() {
        for stale_thread in [false,true] {
            let fixture = fixture(1);
            {
                let mut state = fixture.host.lock().unwrap();
                let view = state.read_model().unwrap().views[0].clone();
                let before = view.panes[0].clone(); let mut pane = before.clone();
                let CardKind::Chat(session) = &mut pane.kind else { unreachable!() };
                if stale_thread { session.launch.session_id = Some("33333333-0000-4000-8000-000000000002".into()); }
                else { session.launch.recipe.environment.insert("OLD_ISSUED_LAUNCH".into(), "another original".into()); }
                state.edit_model(1, ModelEdit::ChangeView { scope: view.scope, expected_revision: view.revision, operation: uuid::Uuid::new_v4(), change: crate::workspace_model::views::Change::Update { before, pane } }).unwrap();
            }
            let before = fixture.host.lock().unwrap().read_model().unwrap();
            std::fs::rename(&fixture.old, &fixture.moved).unwrap();
            let response = super::super::client::process_request(&fixture.host, 1, ClientRequest::EditWorkspaceModel { edit: ModelEdit::RelocateProject { relocation: Relocation { repository: fixture.old.clone(), destination: fixture.moved.clone(), old_root: fixture.old.clone(), new_root: fixture.moved.clone() } } });
            assert!(response.unwrap_err().message.contains("different provider incarnation or issued launch"));
            assert_eq!(fixture.host.lock().unwrap().read_model().unwrap(), before);
            assert!(!fixture.handle.is_completed());
        }
    }

    #[test]
    fn closed_legacy_chat_id_cannot_name_a_fresh_same_recipe_actor() {
        let fixture = fixture(1);
        assert!(!fixture.host.lock().unwrap().issued_chat_ids.contains(&fixture.source.id));
        super::super::client::process_request(&fixture.host, 1, ClientRequest::ChatClose { chat_id: fixture.source.id }).unwrap();
        assert!(!fixture.host.lock().unwrap().chats.contains_key(&fixture.source.id));
        let mut replacement = fixture.source.clone();
        replacement.launch.session_id = None;
        let response = super::super::client::process_request(&fixture.host, 1, ClientRequest::ChatStart { session: replacement, source: None, terminate_source: false, execution_environment: Default::default() });
        assert!(response.unwrap_err().message.contains("session ID was retired"));
        assert!(!fixture.host.lock().unwrap().chats.contains_key(&fixture.source.id));
    }

    #[test]
    fn locate_preserves_an_ordinary_registered_terminal_with_raw_launch_continuity() {
        const CHILD: &str = "CAGENTS_LOCATE_TERMINAL_TEST";
        if std::env::var_os(CHILD).is_none() {
            let directory = tempfile::tempdir().unwrap();
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "host::session_locations::tests::locate_preserves_an_ordinary_registered_terminal_with_raw_launch_continuity", "--nocapture"])
                .env_clear().env(CHILD, "1").env("HOME", directory.path())
                .env("XDG_DATA_HOME", directory.path().join("data")).env("XDG_CONFIG_HOME", directory.path().join("config"))
                .env("XDG_RUNTIME_DIR", directory.path()).env("TMPDIR", directory.path()).env("PATH", "/usr/bin:/bin").spawn().unwrap();
            let deadline = Instant::now() + Duration::from_secs(20);
            loop {
                if let Some(status) = child.try_wait().unwrap() { assert!(status.success(), "registered terminal relocation fixture failed"); return; }
                if Instant::now() >= deadline { child.kill().unwrap(); child.wait().unwrap(); panic!("registered terminal relocation fixture timed out"); }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        let fixture = fixture(0);
        let recipe = LaunchRecipe { program: "/bin/cat".into(), arguments: Vec::new(), working_directory: fixture.old.clone(), environment: Default::default(), unset_environment: Default::default() };
        let terminal = crate::model::TerminalSession { id: TerminalId::new(), owned_codex_listener: None, recovery_origin: None, predecessor: None, launch: recipe.clone(), transcript_path: None, resume_hint: None, continuity: crate::model::TerminalContinuity::RelaunchAvailable { recipe, last_exit: None } };
        { let mut state = fixture.host.lock().unwrap(); state.enable_holder_recovery(); state.register_client(1); }
        let descriptor = super::super::state::spawn_terminal(&fixture.host, 1, terminal.clone(), Default::default()).unwrap();
        struct OwnedTerminal(TerminalHandle);
        impl Drop for OwnedTerminal { fn drop(&mut self) { let _ = self.0.shutdown(); } }
        let owner = OwnedTerminal(fixture.host.lock().unwrap().terminals[&terminal.id].handle.clone());
        let holder = owner.0.holder_identity().expect("actual registered holder");
        {
            let mut state = fixture.host.lock().unwrap();
            assert!(matches!(state.terminals[&terminal.id].session.continuity, crate::model::TerminalContinuity::RelaunchAvailable { .. }));
            let view = state.read_model().unwrap().views[0].clone();
            state.edit_model(1, ModelEdit::ChangeView { scope: view.scope, expected_revision: view.revision, operation: uuid::Uuid::new_v4(), change: crate::workspace_model::views::Change::Insert { pane: Pane { id: CardId::new(), title: "Registered terminal".into(), user_title: None, pending_terminal_start: false, width_fraction: 0.5, kind: CardKind::Terminal(descriptor.session.clone()), scope: Some(fixture.old.clone()) }, before: None } }).unwrap();
        }
        std::fs::rename(&fixture.old, &fixture.moved).unwrap();
        std::fs::create_dir(&fixture.old).unwrap();
        let model = locate(&fixture);
        assert!(model.session_location_refusals.is_empty());
        let state = fixture.host.lock().unwrap();
        assert!(state.terminals[&terminal.id].handle.same_worker(&owner.0));
        assert_eq!(state.terminals[&terminal.id].session.launch, terminal.launch);
        assert_eq!(state.terminals[&terminal.id].handle.holder_identity().unwrap(), holder);
        assert!(holder.root.is_current());
        assert!(!owner.0.is_completed());
        drop(state);
        let registry = super::super::agent_execution::registry(&fixture.host).unwrap();
        assert_eq!(registry.lock().unwrap().session_location(&HostedSession::Terminal(descriptor.session)).unwrap().unwrap().directory, fixture.moved);
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
        assert_eq!(model.session_location_refusals[0].session, fixture.source.id);
        assert!(model.session_location_refusals[0].reason.contains("different directory"));
        assert!(!fixture.handle.is_completed(), "Locate never stops the original provider");
        assert!(fixture.handle.command(chat::Command::Prompt { text: "must not enter replacement".into(), attachments: Vec::new(), mode: chat::PromptMode::Send }).is_err());
        assert!(fixture.calls.lock().unwrap().is_empty());
        assert_eq!(fixture.host.lock().unwrap().chats[&fixture.source.id].session, fixture.source);
        assert!(fixture.moved.is_dir());
        // Restoring the positively identified original directory at the same
        // located spelling allows Verify again without another logical move.
        std::fs::rename(&fixture.moved, fixture._directory.path().join("unapproved-replacement")).unwrap();
        std::fs::rename(&retained, &fixture.moved).unwrap();
        let response = super::super::client::process_request(&fixture.host, 1, ClientRequest::EditWorkspaceModel {
            edit: ModelEdit::RelocateProject { relocation: Relocation { repository: fixture.moved.clone(), destination: fixture.moved.clone(), old_root: fixture.moved.clone(), new_root: fixture.moved.clone() } },
        }).unwrap().0;
        let ServerResponse::WorkspaceModel { model: verified } = response else { panic!() };
        assert!(verified.session_location_refusals.is_empty());
        assert!(verified.sequence > model.sequence);
        fixture.handle.command_for_run(crate::model::RunId::new(), chat::Command::Prompt { text: "follow-up after positive retry".into(), attachments: Vec::new(), mode: chat::PromptMode::Send }).unwrap();
        assert_eq!(*fixture.calls.lock().unwrap(), vec![fixture.moved.clone()]);
    }
}
