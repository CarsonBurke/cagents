//! Owning Codex/terminal holders in an isolated self-ELF child. This fixture
//! uses an inert local protocol peer, not Codex, a provider turn, or desktop119.
use super::*;
use crate::{
    chat::{Launch, NativeContinuity},
    model::{HostIncarnation, HostSessionId, LaunchRecipe, TerminalContinuity, TerminalSession},
    terminal::{TerminalHandle, TerminalSpawnOptions},
};
use serde_json::{Value, json};
use std::{
    fs,
    io::Write,
    os::unix::{fs::PermissionsExt, net::UnixListener},
    path::{Path, PathBuf},
};

const MODE: &str = "CAGENTS_PAIRED_PROJECTION_TEST";
const ROOT: &str = "CAGENTS_PAIRED_PROJECTION_ROOT";
const EXE: &str = "CAGENTS_PAIRED_PROJECTION_EXE";
const SOCKET: &str = "CAGENTS_PAIRED_PROJECTION_SOCKET";
const SELECTOR: &str = "CAGENTS_PAIRED_PROJECTION_SELECTOR";
const CLIENT: u64 = 7;
const THREAD: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";

#[test]
fn current_projection_rebinds_real_paired_native_owner_without_relaunch() {
    if isolated("current_projection_rebinds_real_paired_native_owner_without_relaunch") {
        return;
    }
    let fixture = Fixture::new();
    let raw = fixture.raw();
    assert_ne!(raw.continuity.live_proof().as_ref(), Some(&fixture.proof));
    let provider = fixture.native.root.clone();
    let holder = fixture.native.holder.clone();
    let issued = fixture.source.clone();
    restore(&fixture.host, CLIENT, &issued).unwrap();
    let (published, installed) = {
        let state = fixture.host.lock().unwrap();
        let record = &state.chats[&issued.id];
        assert!(
            record
                .paired_terminal
                .as_ref()
                .unwrap()
                .same_worker(&fixture.worker)
        );
        assert_eq!(record.session.launch.session_id.as_deref(), Some(THREAD));
        assert_eq!(record.session.launch.recipe, issued.launch.recipe);
        assert_eq!(
            record.session.owned_codex_listener,
            issued.owned_codex_listener
        );
        assert_eq!(record.session.proof.as_ref(), Some(&fixture.proof));
        assert_eq!(
            record
                .session
                .terminal
                .as_ref()
                .unwrap()
                .continuity
                .live_proof()
                .as_ref(),
            Some(&fixture.proof)
        );
        (record.session.clone(), record.handle.clone())
    };
    wait_durable(&installed);
    crate::upgrade::handoff::validate_chat_source(&published, &published.launch).unwrap();
    assert_eq!(
        fixture.raw(),
        raw,
        "projection must not rewrite raw custody"
    );
    installed.preserve_for_restart().unwrap();
    let (acknowledged, exited) =
        crate::chat::native_holder::inspect(&fixture.native, &fixture.proof).unwrap();
    assert!(!exited);
    assert_eq!(
        acknowledged.source, published,
        "real holder must have acknowledged the projected source"
    );
    assert_eq!(
        acknowledged.paired_holder.as_ref(),
        fixture.worker.holder_identity()
    );
    assert_eq!(acknowledged.confirmed_thread.as_deref(), Some(THREAD));
    assert_eq!(
        acknowledged.server_identity,
        fixture.original.server_identity
    );
    assert_eq!(acknowledged.server_process, fixture.original.server_process);
    assert!(
        acknowledged.execution_environment == fixture.original.execution_environment,
        "private execution environment changed"
    );
    assert_eq!(
        acknowledged.execution_console,
        fixture.original.execution_console
    );
    assert_eq!(fixture.native.root, provider);
    assert_eq!(fixture.native.holder, holder);
    fixture.assert_one_provider();
    // Reattach through the same supported Host path after read-only inspection.
    restore(&fixture.host, CLIENT, &published).unwrap();
    // A successfully published Local attachment is committed: ordinary close
    // retires the original provider, rather than merely dropping the socket.
    crate::host::chat::process(
        &fixture.host,
        CLIENT,
        crate::terminal::ipc::ClientRequest::ChatClose { chat_id: issued.id },
    )
    .unwrap();
    wait_until(
        || !provider.is_current(),
        "ordinary close did not retire original provider",
    );
}

#[test]
fn paired_projection_requires_exact_holder_launch_listener_and_current_proof() {
    if isolated("paired_projection_requires_exact_holder_launch_listener_and_current_proof") {
        return;
    }
    let fixture = Fixture::new();
    let raw = fixture.raw();
    // Holder-authenticated issued aliases, not caller-edited thread/launch,
    // authorize the real restore path before any pairing projection.
    for change in 0..3 {
        let mut unissued = fixture.source.clone();
        match change {
            0 => unissued.launch.session_id = Some("unissued-thread".into()),
            1 => unissued
                .launch
                .recipe
                .arguments
                .push("unissued-option".into()),
            _ => unissued.terminal.as_mut().unwrap().id = TerminalId::new(),
        }
        assert_eq!(
            restore(&fixture.host, 0, &unissued).unwrap_err(),
            "Native holder does not recognize this exact issued descriptor"
        );
        assert!(
            !fixture
                .host
                .lock()
                .unwrap()
                .chats
                .contains_key(&fixture.source.id)
        );
        assert!(fixture.native.root.is_current());
    }
    let pair = fixture.worker.holder_identity().unwrap().clone();
    let state = fixture.host.lock().unwrap();
    let (projected, worker) =
        project_paired_terminal(&state, &raw, Some(&pair), &fixture.proof).unwrap();
    assert!(worker.same_worker(&fixture.worker));
    assert_eq!(
        projected.continuity.live_proof().as_ref(),
        Some(&fixture.proof)
    );
    assert_eq!(state.terminals[&raw.id].session, raw);
    assert!(project_paired_terminal(&state, &raw, None, &fixture.proof).is_err());
    let mut changed = pair.clone();
    changed.holder.start += 1;
    assert!(project_paired_terminal(&state, &raw, Some(&changed), &fixture.proof).is_err());
    changed = pair.clone();
    changed.root.start += 1;
    assert!(project_paired_terminal(&state, &raw, Some(&changed), &fixture.proof).is_err());
    changed = pair.clone();
    changed.boot = crate::model::BootId::new("other-boot");
    assert!(project_paired_terminal(&state, &raw, Some(&changed), &fixture.proof).is_err());
    changed = pair.clone();
    changed.terminal = TerminalId::new();
    assert!(project_paired_terminal(&state, &raw, Some(&changed), &fixture.proof).is_err());
    changed = pair.clone();
    changed.kind = Kind::Native;
    assert!(project_paired_terminal(&state, &raw, Some(&changed), &fixture.proof).is_err());
    let mut input = raw.clone();
    input.id = TerminalId::new();
    assert!(project_paired_terminal(&state, &input, Some(&pair), &fixture.proof).is_err());
    input = raw.clone();
    input.launch.arguments.push("not-the-launch".into());
    assert!(project_paired_terminal(&state, &input, Some(&pair), &fixture.proof).is_err());
    input = raw.clone();
    input.owned_codex_listener = fixture.source.owned_codex_listener.clone();
    assert_ne!(input.owned_codex_listener, raw.owned_codex_listener);
    assert!(project_paired_terminal(&state, &input, Some(&pair), &fixture.proof).is_err());
    let mut proof = fixture.proof.clone();
    proof.host_incarnation = HostIncarnation::new("not-current");
    assert!(project_paired_terminal(&state, &raw, Some(&pair), &proof).is_err());
    proof = fixture.proof.clone();
    proof.boot_id = crate::model::BootId::new("other-boot");
    assert!(project_paired_terminal(&state, &raw, Some(&pair), &proof).is_err());
    proof = fixture.proof.clone();
    proof.host_session_id = HostSessionId::new("");
    assert!(project_paired_terminal(&state, &raw, Some(&pair), &proof).is_err());
    assert!(pair_projection_is_current(&state, None, None));
    assert!(!pair_projection_is_current(&state, Some(&projected), None));
    assert!(!pair_projection_is_current(&state, None, Some(&worker)));
    assert!(pair_projection_is_current(
        &state,
        Some(&projected),
        Some(&worker)
    ));
    drop(state);
    fixture
        .worker
        .close_with_receipt(&projected, &fixture.proof)
        .unwrap();
    wait_until(
        || fixture.worker.is_completed(),
        "terminal did not complete",
    );
    let state = fixture.host.lock().unwrap();
    assert!(project_paired_terminal(&state, &raw, Some(&pair), &fixture.proof).is_err());
    assert!(!pair_projection_is_current(
        &state,
        Some(&projected),
        Some(&worker)
    ));
}

#[test]
fn refused_source_update_ack_preserves_exact_holder_thread_and_provider() {
    if isolated("refused_source_update_ack_preserves_exact_holder_thread_and_provider") {
        return;
    }
    let fixture = Fixture::new();
    // Exercise the actual owning framed transport, not Recovery's private
    // reducer: a refused UpdateSource is not a SourceUpdated acknowledgment.
    let (mut holder, recovered) =
        crate::chat::native_holder::adopt(&fixture.native, &fixture.source, &fixture.proof)
            .unwrap();
    let mut attempted = recovered.source.clone();
    attempted.terminal = Some(
        fixture
            .host
            .lock()
            .unwrap()
            .native_terminal_projection(attempted.id)
            .unwrap(),
    );
    attempted.launch.session_id = Some("different-thread".into());
    let refusal = holder
        .update_source(
            attempted,
            Some(fixture.raw().launch),
            fixture.worker.holder_identity().cloned(),
        )
        .unwrap_err();
    assert_eq!(
        refusal,
        "Paired view update changed native conversation ownership"
    );
    let _detached = holder.preserve();
    let (after, exited) =
        crate::chat::native_holder::inspect(&fixture.native, &fixture.proof).unwrap();
    assert!(!exited);
    assert_eq!(after.source, recovered.source);
    assert_eq!(after.confirmed_thread.as_deref(), Some(THREAD));
    assert_eq!(after.paired_holder, recovered.paired_holder);
    assert!(
        !fixture
            .host
            .lock()
            .unwrap()
            .chats
            .contains_key(&fixture.source.id)
    );
    assert!(fixture.native.root.is_current());
    assert!(fixture.native.holder.is_current());
    assert_eq!(
        fs::read_to_string(fixture.root.join("provider-launches")).unwrap(),
        "launch\n"
    );
}

#[test]
fn final_projected_field_change_refuses_without_releasing_original_provider() {
    if isolated("final_projected_field_change_refuses_without_releasing_original_provider") {
        return;
    }
    let fixture = Fixture::new();
    let id = fixture.source.id;
    let _hook = before_publication_for_test(move |host| {
        host.lock()
            .unwrap()
            .terminals
            .get_mut(&id)
            .unwrap()
            .session
            .transcript_path = Some(PathBuf::from("changed-after-source-ack"));
    });
    let refusal = restore(&fixture.host, 0, &fixture.source).unwrap_err();
    assert_eq!(
        refusal,
        "Native adoption changed before attachment publication"
    );
    fixture.assert_unpublished_and_preserved();
    let (acknowledged, _) =
        crate::chat::native_holder::inspect(&fixture.native, &fixture.proof).unwrap();
    assert_eq!(
        acknowledged
            .source
            .terminal
            .as_ref()
            .unwrap()
            .transcript_path,
        None,
        "actual ACK preceded the final mutation"
    );
}

#[test]
fn final_equal_projection_worker_replacement_refuses_without_releasing_provider() {
    if isolated("final_equal_projection_worker_replacement_refuses_without_releasing_provider") {
        return;
    }
    let mut fixture = Fixture::new();
    let id = fixture.source.id;
    let (replacement, replacement_worker) = fixture.extra_terminal();
    let replacement_id = replacement.id;
    let captured = fixture
        .host
        .lock()
        .unwrap()
        .native_terminal_projection(id)
        .unwrap();
    assert!(!replacement_worker.same_worker(&fixture.worker));
    let _hook = before_publication_for_test(move |host| {
        let mut state = host.lock().unwrap();
        let mut replacement = state.terminals.remove(&replacement_id).unwrap();
        // Fixture-only ABA: equal full projection with a genuinely different
        // owned worker. The private worker check must still refuse it.
        replacement.session = captured;
        state.terminals.insert(id, replacement);
    });
    let refusal = restore(&fixture.host, 0, &fixture.source).unwrap_err();
    assert_eq!(
        refusal,
        "Native adoption changed before attachment publication"
    );
    fixture.assert_unpublished_and_preserved();
    assert!(
        replacement_worker
            .holder_identity()
            .unwrap()
            .root
            .is_current()
    );
}

#[test]
fn fresh_projection_rechecks_full_session_and_host_proof() {
    if isolated("fresh_projection_rechecks_full_session_and_host_proof") {
        return;
    }
    let fixture = Fixture::new();
    let id = fixture.source.id;
    let mut state = fixture.host.lock().unwrap();
    let expected = state.native_terminal_projection(id).unwrap();
    let raw = state.terminals[&id].session.clone();
    for change in 0..5 {
        let current = &mut state.terminals.get_mut(&id).unwrap().session;
        match change {
            0 => current.launch.arguments.push("changed".into()),
            1 => current.transcript_path = Some(PathBuf::from("different-transcript")),
            2 => current.owned_codex_listener = fixture.source.owned_codex_listener.clone(),
            3 => current.predecessor = Some(TerminalId::new()),
            _ => current.recovery_origin = Some(TerminalId::new()),
        }
        assert!(
            !pair_projection_is_current(&state, Some(&expected), Some(&fixture.worker)),
            "full descriptor change {change} escaped"
        );
        state.terminals.get_mut(&id).unwrap().session = raw.clone();
    }
    // Raw continuity is intentionally not authoritative, for all retained
    // forms: the same already-authenticated worker supplies the projection.
    for continuity in [
        TerminalContinuity::RelaunchAvailable {
            recipe: raw.launch.clone(),
            last_exit: None,
        },
        TerminalContinuity::DetachedSameBoot {
            host_session_id: fixture.old_proof.host_session_id.clone(),
            boot_id: fixture.old_proof.boot_id.clone(),
            host_incarnation: fixture.old_proof.host_incarnation.clone(),
        },
        raw.continuity.clone(),
    ] {
        state.terminals.get_mut(&id).unwrap().session.continuity = continuity;
        assert!(pair_projection_is_current(
            &state,
            Some(&expected),
            Some(&fixture.worker)
        ));
    }
    state.terminals.get_mut(&id).unwrap().session = raw;
    let record = state.terminals.remove(&id).unwrap();
    assert!(!pair_projection_is_current(
        &state,
        Some(&expected),
        Some(&fixture.worker)
    ));
    let mut wrong = HostState::new(HostSessionProof {
        host_incarnation: HostIncarnation::new("replaced-host"),
        ..fixture.proof.clone()
    });
    wrong.terminals.insert(id, record);
    assert!(!pair_projection_is_current(
        &wrong,
        Some(&expected),
        Some(&fixture.worker)
    ));
    state
        .terminals
        .insert(id, wrong.terminals.remove(&id).unwrap());
}

// Installed before the first fallible provider wait, so partial construction
// retains the actual original handles and authenticated cleanup route too.
#[test]
fn confirmed_reconnect_only_actor_is_not_a_source_update_acknowledgment() {
    if isolated("confirmed_reconnect_only_actor_is_not_a_source_update_acknowledgment") {
        return;
    }
    use crate::chat::protocol::{Adapter, ProcessSpec, Step};
    struct ConfirmOnly;
    impl Adapter for ConfirmOnly {
        fn initialize(&mut self) -> Result<Step, String> {
            Ok(Step {
                writes: vec![json!({"fixture":true})],
                ..Step::default()
            })
        }
        fn command(&mut self, _: crate::chat::Command) -> Result<Step, String> {
            Err("no fixture commands".into())
        }
        fn receive(&mut self, _: Value) -> Result<Step, String> {
            Ok(Step {
                events: vec![crate::chat::Event::Session {
                    id: THREAD.into(),
                    title: None,
                }],
                ..Step::default()
            })
        }
    }
    let root = PathBuf::from(std::env::var_os(ROOT).unwrap());
    let recipe = LaunchRecipe {
        program: "/bin/cat".into(),
        arguments: Vec::new(),
        working_directory: root.join("project"),
        environment: Default::default(),
        unset_environment: Default::default(),
    };
    let spec = ProcessSpec::from(&recipe);
    let mut adapter: Box<dyn Adapter> = Box::new(ConfirmOnly);
    let mut initial = adapter.initialize().unwrap();
    let store = crate::chat::runtime::preflight(&spec, &mut initial).unwrap();
    let handle = Handle::start(spec, adapter, initial, store, None, |_| {}).unwrap();
    struct Close(Handle);
    impl Drop for Close {
        fn drop(&mut self) {
            let _ = self.0.close();
        }
    }
    let _close = Close(handle.clone());
    wait_until(
        || handle.confirmed_session_id().as_deref() == Some(THREAD),
        "local echo never confirmed its thread",
    );
    assert_eq!(handle.native_continuity(), NativeContinuity::ReconnectOnly);
    let source = Session {
        id: TerminalId::new(),
        launch: Launch {
            provider: Provider::Codex,
            recipe,
            session_id: Some(THREAD.into()),
        },
        proof: None,
        terminal: None,
        owned_codex_listener: None,
        native_continuity: NativeContinuity::ReconnectOnly,
    };
    // This is the actual runtime no-op that must not be mistaken for a holder
    // SourceUpdated response during the two independently published states.
    handle.update_native_source(source, None, None).unwrap();
    assert!(!restored_thread_ready(&handle, Some(THREAD)).unwrap());
    assert_eq!(
        restored_thread_ready(&handle, Some("other-thread")).unwrap_err(),
        "Retained server confirmed a different conversation"
    );
    assert_eq!(
        restored_thread_ready(&handle, None).unwrap_err(),
        "Retained server confirmed a different conversation"
    );
}

struct ConstructionCleanup {
    host: SharedHost,
    actor: Option<Handle>,
    source: Option<Session>,
    native: Option<registry::Record>,
    terminals: Vec<(TerminalSession, TerminalHandle)>,
    proof: HostSessionProof,
    armed: bool,
}

impl Drop for ConstructionCleanup {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Some(actor) = &self.actor {
            let _ = actor.close();
        }
        let current: Vec<_> = {
            let state = self.host.lock().unwrap_or_else(|p| p.into_inner());
            state
                .terminals
                .iter()
                .filter_map(|(id, record)| {
                    state
                        .native_terminal_projection(*id)
                        .ok()
                        .map(|source| (source, record.handle.clone()))
                })
                .collect()
        };
        for (source, worker) in &current {
            if !worker.is_completed() {
                let _ = worker.close_with_receipt(source, &self.proof);
            }
        }
        for (source, worker) in &self.terminals {
            if !worker.is_completed() {
                let _ = worker.close_with_receipt(source, &self.proof);
            }
        }
        if let (Some(native), Some(source)) = (&self.native, &self.source)
            && native.root.is_current()
            && let Ok((mut holder, _)) =
                crate::chat::native_holder::adopt(native, source, &self.proof)
        {
            let _ = holder.close();
        }
        // The host Arc keeps construction's exact terminal/actor handles alive
        // until the above bounded cleanup attempts finish.
        let _ = &self.host;
    }
}

struct Fixture {
    host: SharedHost,
    source: Session,
    native: registry::Record,
    original: crate::chat::native_holder::Recovery,
    worker: TerminalHandle,
    terminals: Vec<(TerminalSession, TerminalHandle)>,
    proof: HostSessionProof,
    old_proof: HostSessionProof,
    _lease: crate::daemon::HostLease,
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = PathBuf::from(std::env::var_os(ROOT).unwrap());
        let old_proof = HostSessionProof {
            host_session_id: HostSessionId::new("paired-projection-fixture"),
            host_incarnation: HostIncarnation::new("original"),
            boot_id: crate::daemon::read_boot_id().unwrap(),
        };
        let paths = crate::daemon::RuntimePaths::discover().unwrap();
        let mut lease = Some(crate::daemon::HostLease::acquire(&paths, &old_proof).unwrap());
        let mut state = HostState::new(old_proof.clone());
        state.register_client(0);
        state.register_client(CLIENT);
        state.enable_holder_recovery();
        let original = Arc::new(crate::host::state::HostLock::new(state));
        let program = install_provider(&root);
        let environment = [
            (ROOT.into(), root.to_string_lossy().into_owned()),
            (
                SELECTOR.into(),
                fs::read_to_string(root.join("test-name")).unwrap(),
            ),
            (
                EXE.into(),
                std::env::current_exe()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
            ),
            (
                "HOME".into(),
                root.join("home").to_string_lossy().into_owned(),
            ),
            (
                "CODEX_HOME".into(),
                root.join("home/.codex").to_string_lossy().into_owned(),
            ),
            ("PATH".into(), "/usr/bin:/bin".into()),
        ]
        .into_iter()
        .collect();
        let mut source = Session {
            id: TerminalId::new(),
            launch: Launch {
                provider: Provider::Codex,
                recipe: LaunchRecipe {
                    program: program.to_string_lossy().into_owned(),
                    arguments: vec!["--remote".into(), "unix://".into()],
                    working_directory: root.join("project"),
                    environment,
                    unset_environment: Default::default(),
                },
                session_id: None,
            },
            proof: None,
            terminal: None,
            owned_codex_listener: None,
            native_continuity: Default::default(),
        };
        assert!(source.claim_owned_codex_listener(&root, &old_proof));
        let id = source.id;
        crate::host::chat::start_execution(
            &original,
            0,
            source,
            Arc::new(crate::chat::protocol::ExecutionContext::new(Some(
                Arc::new(fs::File::open(root.join("project")).unwrap()),
            ))),
        )
        .unwrap();
        let actor = original.lock().unwrap().chats[&id].handle.clone();
        let mut cleanup = ConstructionCleanup {
            host: original.clone(),
            actor: Some(actor.clone()),
            source: None,
            native: None,
            terminals: Vec::new(),
            proof: old_proof.clone(),
            armed: true,
        };
        wait_durable(&actor);
        let ownership = root.join("registry.sqlite3");
        original
            .lock()
            .unwrap()
            .install_model(crate::host::model::Registry::at(&ownership).unwrap());
        let initial = original.lock().unwrap().chats[&id].session.clone();
        let ServerResponse::ChatAttached {
            session: source, ..
        } = session_snapshot(&initial, &actor).unwrap()
        else {
            panic!("actual Host attachment snapshot required")
        };
        cleanup.source = Some(source.clone());
        crate::host::agent_execution::registry(&original)
            .unwrap()
            .lock()
            .unwrap()
            .fresh_agent_authority(&crate::agent_execution::HostedSession::Chat(source.clone()))
            .unwrap();
        let recipe = LaunchRecipe {
            program: "/bin/cat".into(),
            arguments: Vec::new(),
            working_directory: root.join("project"),
            environment: Default::default(),
            unset_environment: Default::default(),
        };
        original
            .lock()
            .unwrap()
            .chats
            .get_mut(&id)
            .unwrap()
            .console_recipe = Some(recipe.clone());
        let terminal = terminal_source(id, recipe);
        let (descriptor, worker) = crate::host::state::spawn_terminal_view_owned(
            &original,
            0,
            terminal,
            TerminalSpawnOptions::default(),
            None,
            true,
        )
        .unwrap();
        let native = registry::evidence(
            &registry::socket(&registry::directory(Kind::Native).unwrap(), id),
            Kind::Native,
        )
        .unwrap()
        .unwrap();
        cleanup.native = Some(native.clone());
        cleanup
            .terminals
            .push((descriptor.session.clone(), worker.clone()));
        let original_pair = worker.holder_identity().unwrap().clone();
        let mut source = source;
        source.terminal = Some(descriptor.session.clone());
        actor
            .update_native_source(
                source.clone(),
                Some(descriptor.session.launch.clone()),
                worker.holder_identity().cloned(),
            )
            .unwrap();
        {
            let mut state = original.lock().unwrap();
            let record = state.chats.get_mut(&id).unwrap();
            record.session = source.clone();
            record.console_recipe = Some(descriptor.session.launch.clone());
            record.paired_terminal = Some(worker.clone());
        }
        cleanup.source = Some(source.clone());
        actor.preserve_for_restart().unwrap();
        cleanup.actor = None;
        let (acknowledged, _) = crate::chat::native_holder::inspect(&native, &old_proof).unwrap();
        assert_eq!(acknowledged.source, source);
        assert_eq!(acknowledged.paired_holder.as_ref(), Some(&original_pair));
        let retained = acknowledged
            .previous_sources
            .iter()
            .chain(std::iter::once(&acknowledged.source))
            .cloned()
            .map(crate::agent_execution::HostedSession::Chat)
            .collect::<Vec<_>>();
        crate::host::agent_execution::registry(&original)
            .unwrap()
            .lock()
            .unwrap()
            .adopt_authenticated_agent_session(
                &retained,
                &crate::agent_execution::HostedSession::Chat(source.clone()),
            )
            .unwrap();
        worker.preserve_for_restart().unwrap();
        drop(actor);
        drop(original);
        drop(lease.take());
        let proof = HostSessionProof {
            host_incarnation: HostIncarnation::new("successor"),
            ..old_proof.clone()
        };
        lease = Some(crate::daemon::HostLease::acquire(&paths, &proof).unwrap());
        cleanup.proof = proof.clone();
        let mut state = HostState::new(proof.clone());
        state.install_model(crate::host::model::Registry::at(&ownership).unwrap());
        state.register_client(0);
        state.register_client(CLIENT);
        state.enable_holder_recovery();
        let host = Arc::new(crate::host::state::HostLock::new(state));
        cleanup.host = host.clone();
        crate::host::state::restore_orphaned_holders(&host).unwrap();
        let worker = host.lock().unwrap().terminals[&id].handle.clone();
        assert_eq!(
            worker.holder_identity(),
            Some(&original_pair),
            "terminal restore must retain exact original holder"
        );
        let cleanup_source = host.lock().unwrap().native_terminal_projection(id).unwrap();
        cleanup.armed = false;
        Self {
            host,
            source,
            native,
            original: acknowledged,
            worker: worker.clone(),
            terminals: vec![(cleanup_source, worker)],
            proof,
            old_proof,
            _lease: lease.take().unwrap(),
            root,
        }
    }

    fn raw(&self) -> TerminalSession {
        self.host.lock().unwrap().terminals[&self.source.id]
            .session
            .clone()
    }

    fn extra_terminal(&mut self) -> (TerminalSession, TerminalHandle) {
        let terminal = terminal_source(TerminalId::new(), self.raw().launch);
        let (descriptor, worker) = crate::host::state::spawn_terminal_view_owned(
            &self.host,
            0,
            terminal,
            TerminalSpawnOptions::default(),
            None,
            true,
        )
        .unwrap();
        self.terminals
            .push((descriptor.session.clone(), worker.clone()));
        (descriptor.session, worker)
    }

    fn assert_one_provider(&self) {
        assert!(self.native.root.is_current());
        assert!(self.native.holder.is_current());
        assert_eq!(
            fs::read_to_string(self.root.join("provider-launches")).unwrap(),
            "launch\n"
        );
        let requests = fs::read_to_string(self.root.join("provider-requests")).unwrap();
        assert_eq!(
            requests
                .lines()
                .filter(|method| *method == "thread/start")
                .count(),
            1
        );
        assert!(requests.lines().any(|method| method == "thread/resume"));
        assert!(!requests.lines().any(|method| method.starts_with("turn/")));
    }

    fn assert_unpublished_and_preserved(&self) {
        assert!(
            !self
                .host
                .lock()
                .unwrap()
                .chats
                .contains_key(&self.source.id)
        );
        assert!(
            !self
                .host
                .lock()
                .unwrap()
                .chat_spawning
                .contains_key(&self.source.id)
        );
        self.assert_one_provider();
        let (recovered, exited) =
            crate::chat::native_holder::inspect(&self.native, &self.proof).unwrap();
        assert!(
            !exited,
            "failed final publication released the original provider"
        );
        assert_eq!(recovered.server_process, self.original.server_process);
        assert_eq!(recovered.server_identity, self.original.server_identity);
        assert_eq!(recovered.confirmed_thread.as_deref(), Some(THREAD));
        assert!(self.worker.holder_identity().unwrap().root.is_current());
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // The same actual owners are retained even on assertion failure. Never
        // search for a provider, signal a guessed PID, or launch a replacement.
        let actor = self
            .host
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .chats
            .get(&self.source.id)
            .map(|record| record.handle.clone());
        if let Some(actor) = actor {
            let _ = actor.close();
        }
        for (source, worker) in &self.terminals {
            if !worker.is_completed() {
                let _ = worker.close_with_receipt(source, &self.proof);
            }
        }
        if self.native.root.is_current()
            && let Ok((mut holder, _)) =
                crate::chat::native_holder::adopt(&self.native, &self.source, &self.proof)
        {
            let _ = holder.close();
        }
    }
}

fn terminal_source(id: TerminalId, recipe: LaunchRecipe) -> TerminalSession {
    TerminalSession {
        id,
        launch: recipe.clone(),
        continuity: TerminalContinuity::RelaunchAvailable {
            recipe,
            last_exit: None,
        },
        resume_hint: None,
        transcript_path: None,
        owned_codex_listener: None,
        recovery_origin: None,
        predecessor: None,
    }
}

fn wait_until(mut condition: impl FnMut() -> bool, message: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !condition() {
        assert!(Instant::now() < deadline, "{message}");
        thread::sleep(Duration::from_millis(10));
    }
}

fn wait_durable(handle: &crate::chat::runtime::Handle) {
    wait_until(
        || {
            matches!(handle.native_continuity(), NativeContinuity::Durable { .. })
                && handle.check_update_readiness()
                    == Ok(crate::chat::runtime::UpdateReadiness::Idle)
        },
        "provider never became durable and idle",
    );
    assert_eq!(handle.confirmed_session_id().as_deref(), Some(THREAD));
}

/// True means this invocation was only the bounded outer driver/provider role.
fn isolated(name: &str) -> bool {
    match std::env::var(MODE).as_deref() {
        Ok("provider") => {
            provider();
            return true;
        }
        Ok("fixture") => return false,
        _ => {}
    }
    use crate::process::{CancellationToken, CommandSpec, ProcessExecutor, SystemProcessExecutor};
    let directory = tempfile::tempdir_in("/var/tmp").unwrap();
    let root = directory.path();
    fs::set_permissions(root, fs::Permissions::from_mode(0o700)).unwrap();
    for path in [
        "home/.codex",
        "project",
        "run",
        "data",
        "config",
        "cache",
        "bin",
        "tmp",
    ] {
        fs::create_dir_all(root.join(path)).unwrap();
    }
    let exe = std::env::current_exe().unwrap();
    fs::write(
        root.join("test-name"),
        format!("host::chat::native_recovery::paired_projection_tests::{name}"),
    )
    .unwrap();
    let mut command = CommandSpec::new(&exe);
    // Retain opaque inherited delegation and the actual same-head helper.
    // Neither value enters the provider recipe, logs, or a replacement token.
    for (key, _) in std::env::vars_os() {
        if key != std::ffi::OsStr::new(crate::agent_execution::AUTHORITY_ENV)
            && key != std::ffi::OsStr::new("CAGENTS_TEST_RETAINED_ACTOR_HELPER")
        {
            command = command.env_remove(key);
        }
    }
    let command = command
        .args([
            "--exact",
            &format!("host::chat::native_recovery::paired_projection_tests::{name}"),
            "--nocapture",
            "--test-threads=1",
        ])
        .cwd(root)
        .env(MODE, "fixture")
        .env(ROOT, root)
        .env(EXE, &exe)
        .env("HOME", root.join("home"))
        .env("CODEX_HOME", root.join("home/.codex"))
        .env("XDG_RUNTIME_DIR", root.join("run"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("TMPDIR", root.join("tmp"))
        .env("PATH", "/usr/bin:/bin")
        .timeout(Duration::from_secs(90))
        .output_limits(1024 * 1024, 1024 * 1024);
    let result = SystemProcessExecutor.execute(&command, &CancellationToken::new());
    let (out, err) = match &result {
        Ok(output) => (&output.stdout, &output.stderr),
        Err(error) => (&error.stdout, &error.stderr),
    };
    fs::write(root.join("child-stdout"), out).unwrap();
    fs::write(root.join("child-stderr"), err).unwrap();
    let retained = directory.keep();
    let output = result.expect("bounded original fixture child must settle");
    assert!(
        output.success,
        "paired projection child failed at {}:\n{}\n{}",
        retained.display(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed"));
    true
}

fn install_provider(root: &Path) -> PathBuf {
    let program = root.join("bin/codex");
    fs::write(
        &program,
        format!(
            r#"#!/bin/sh
set -eu
for arg in "$@"; do
  case "$arg" in unix://*) export {SOCKET}="${{arg#unix://}}" ;; esac
done
echo launch >> "${ROOT}/provider-launches"
export {MODE}=provider
exec "${EXE}" --exact "${SELECTOR}" --nocapture --test-threads=1
"#
        ),
    )
    .unwrap();
    fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
    program
}

fn provider() {
    let root = PathBuf::from(std::env::var_os(ROOT).unwrap());
    let listener = UnixListener::bind(std::env::var_os(SOCKET).unwrap()).unwrap();
    listener.set_nonblocking(true).unwrap();
    let deadline = Instant::now() + Duration::from_secs(75);
    while Instant::now() < deadline {
        let (stream, _) = match listener.accept() {
            Ok(accepted) => accepted,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(5));
                continue;
            }
            Err(error) => panic!("private provider accept: {error}"),
        };
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut websocket = tungstenite::accept(stream).unwrap();
        while Instant::now() < deadline {
            let message = match websocket.read() {
                Ok(message) => message,
                Err(tungstenite::Error::Io(error))
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    continue;
                }
                Err(_) => break,
            };
            if message.is_close() {
                break;
            }
            let Ok(text) = message.to_text() else {
                continue;
            };
            let frame: Value = serde_json::from_str(text).unwrap();
            let Some(id) = frame.get("id") else { continue };
            let method = frame["method"].as_str().unwrap();
            let mut log = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(root.join("provider-requests"))
                .unwrap();
            writeln!(log, "{method}").unwrap();
            let result = match method {
                "initialize" => {
                    json!({"userAgent":"cagents/0.157.1 (Linux)", "codexHome":std::env::var("CODEX_HOME").unwrap()})
                }
                "thread/start" | "thread/resume" => {
                    json!({"thread":{"id":THREAD,"canAcceptDirectInput":true,"status":{"type":"idle"}},"model":"fixture-model"})
                }
                "thread/read" => json!({"thread":{"id":THREAD,"turns":[]}}),
                "thread/turns/list" | "thread/queue/list" | "thread/list" => {
                    json!({"data":[],"nextCursor":null})
                }
                "model/list" | "skills/list" => json!({"data":[]}),
                _ => panic!("unexpected provider request {method}"),
            };
            if websocket
                .send(tungstenite::Message::Text(
                    json!({"id":id,"result":result}).to_string().into(),
                ))
                .is_err()
            {
                break;
            }
        }
    }
    panic!("private provider exceeded its finite fixture lifetime");
}
