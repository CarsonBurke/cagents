//! A stopped update pane pages the reviewed transcript without a provider.
use super::*;
use crate::chat::protocol::{Adapter, ProcessSpec, Step, TransportSpec};
use std::path::Path;

/// Production recovery always inspects the real process table. Unit fixtures
/// substitute only the proc filesystem, retaining the normal survivor parser.
fn resume_process_root() -> std::path::PathBuf {
    #[cfg(test)]
    if let Some(root) = RESUME_PROCESS_ROOT.with(|slot| slot.borrow().clone()) {
        return root;
    }
    "/proc".into()
}

#[cfg(test)]
thread_local! {
    static RESUME_PROCESS_ROOT: std::cell::RefCell<Option<std::path::PathBuf>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
struct ProcessRootGuard(Option<std::path::PathBuf>);

#[cfg(test)]
impl ProcessRootGuard {
    fn install(root: &std::path::Path) -> Self {
        Self(RESUME_PROCESS_ROOT.with(|slot| slot.replace(Some(root.to_owned()))))
    }
}

#[cfg(test)]
impl Drop for ProcessRootGuard {
    fn drop(&mut self) {
        RESUME_PROCESS_ROOT.with(|slot| {
            slot.replace(self.0.take());
        });
    }
}

struct StoppedAdapter;
impl Adapter for StoppedAdapter {
    fn initialize(&mut self) -> Result<Step, String> {
        Ok(Step::default())
    }
    fn command(&mut self, _command: chat::Command) -> Result<Step, String> {
        Err(
            "This transcript is read-only. Start a new conversation to continue, or use Resume if a conversation was recorded."
                .into(),
        )
    }
    fn receive(&mut self, _frame: serde_json::Value) -> Result<Step, String> {
        Err("A stopped update transcript has no provider transport".into())
    }
}

pub(super) fn recover(host: &SharedHost, requested: &Session) -> Result<(), String> {
    let ledger = crate::upgrade::floor::load()?;
    let original = ledger
        .resume_source(requested)
        .ok_or("The stopped update source changed its saved conversation")?;
    let saved = crate::upgrade::floor::snapshot(original)?;
    install(host, &saved, requested)
}

pub(super) fn recover_attachment(host: &SharedHost, requested: &Session) -> Result<bool, String> {
    // A malformed floor ledger must leave ordinary provider-saved history
    // attachable through its own proof and transcript.
    let Ok(ledger) = crate::upgrade::floor::load() else {
        return Ok(false);
    };
    let Some(original) = ledger
        .resume_source(requested)
        .filter(|original| ledger.history_replacement(original, requested))
    else {
        return Ok(false);
    };
    install(host, &crate::upgrade::floor::snapshot(original)?, requested)?;
    Ok(true)
}

fn projection(
    host: &SharedHost,
    saved: &crate::upgrade::floor::StoppedSnapshot,
    session: &Session,
) -> Result<(ProcessSpec, Step, chat::runtime::Store), String> {
    let mut spec = ProcessSpec::from(&session.launch.recipe);
    spec.transport = TransportSpec::History;
    spec.messages = crate::host::agent_execution::mailbox::recognizer(host, saved.source.id);
    let mut events = vec![chat::Event::History {
        items: saved
            .snapshot
            .items
            .iter()
            .map(|item| (**item).clone())
            .collect(),
        has_earlier: false,
    }];
    if let Some(id) = &session.launch.session_id {
        events.push(chat::Event::Session {
            id: id.clone(),
            title: saved.snapshot.title.clone(),
        });
    }
    events.push(chat::Event::ReadOnlyHistory {
        reason: saved
            .limitation
            .clone()
            .unwrap_or_else(|| crate::upgrade::floor::default_limitation(&saved.source).into()),
    });
    events.push(chat::Event::Exited { code: None });
    let mut initial = Step {
        events,
        ..Step::default()
    };
    let store = chat::runtime::preflight(&spec, &mut initial)?;
    Ok((spec, initial, store))
}

pub(super) fn reload(
    host: &SharedHost,
    session: &Session,
) -> Result<Option<chat::runtime::Store>, String> {
    // install records the exact captured source first, before any issued
    // aliases. Several update epochs can name the same allocation.
    let original = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .upgrade_chats
        .get(&session.id)
        .and_then(|sources| sources.first())
        .map(|entry| entry.source.clone());
    let Some(original) = original else {
        return Ok(None);
    };
    let ledger = crate::upgrade::floor::load()?;
    if !ledger.history_replacement(&original, session)
        || ledger.resume_source(&original) != Some(&original)
    {
        return Ok(None);
    }
    let saved = crate::upgrade::floor::snapshot(&original)?;
    projection(host, &saved, session).map(|(_, _, store)| Some(store))
}

fn install(
    host: &SharedHost,
    saved: &crate::upgrade::floor::StoppedSnapshot,
    requested: &Session,
) -> Result<(), String> {
    let proof = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .proof()
        .clone();
    {
        let id = saved.source.id;
        let mut session = saved.source.clone();
        session.proof = Some(proof.clone());
        session.reset_owned_codex_listener();
        session.native_continuity = chat::NativeContinuity::ReconnectOnly;
        session.terminal = None;
        session.owned_codex_listener = None;
        let mut admission = None;
        {
            let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
            if state.chats.contains_key(&id) {
                return Ok(());
            }
            if state.shutting_down
                || state.chat_spawning.contains_key(&id)
                || state.native_closing.contains_key(&id)
                || state.native_actor_startups.contains_key(&id)
                || state.chat_handoffs.contains_key(&id)
            {
                return Err("The conversation is changing; its saved history was kept".into());
            }
            capacity::Reservation::install(&mut admission, host, &mut state, &session)
                .map_err(|error| error.message)?;
            state.inventory_changed();
        }
        let (spec, initial, store) = projection(host, saved, &session)?;
        let environment = spec.environment.clone();
        let weak = Arc::downgrade(host);
        let handle = Handle::start_owned(
            spec,
            Box::new(StoppedAdapter),
            initial,
            store,
            session.clone(),
            environment,
            None,
            move |update| changed(&weak, id, update),
        )?;
        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
        if state.shutting_down
            || state.chats.contains_key(&id)
            || !admission
                .as_ref()
                .is_some_and(|admission| admission.holds(&state, id))
        {
            drop(state);
            handle.close()?;
            return Err("The conversation changed while its saved history opened".into());
        }
        let mut sources = vec![crate::upgrade::handoff::ChatSource {
            source: saved.source.clone(),
            launch: saved.source.launch.clone(),
        }];
        if requested != &saved.source {
            sources.push(crate::upgrade::handoff::ChatSource {
                source: requested.clone(),
                launch: requested.launch.clone(),
            });
        }
        state.upgrade_chats.insert(id, sources);
        state.chats.insert(
            id,
            Record {
                session,
                handle,
                execution_instance: uuid::Uuid::new_v4(),
                console_recipe: None,
                paired_terminal: None,
                source_process: None,
                subscribers: BTreeMap::new(),
                observations: BTreeMap::new(),
                completion_observed: false,
                returning_terminal: false,
            },
        );
        state.chat_added(id);
        state.inventory_changed();
    }
    Ok(())
}

/// A Resume click is a separate writer admission. Re-scan the old exact
/// conversation immediately before allocating its replacement; the stop choice
/// itself is never evidence that teardown succeeded.
pub(super) fn prepare_resume(
    host: &SharedHost,
    requested: &Session,
) -> Result<Option<Session>, String> {
    let ledger = crate::upgrade::floor::load()?;
    let Some(source) = ledger.resume_source(requested) else {
        return Ok(None);
    };
    let conversation = source
        .launch
        .session_id
        .as_deref()
        .ok_or("This pane has no recorded conversation; start a new conversation instead")?;
    let proof = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .proof()
        .clone();
    {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        if state.shutting_down
            || state.chat_spawning.contains_key(&source.id)
            || state.native_closing.contains_key(&source.id)
            || state.chats.values().any(|record| {
                record.handle.owns_work()
                    && record.session.launch.provider == source.launch.provider
                    && record.handle.session_id().as_deref() == Some(conversation)
            })
        {
            return Err("This conversation still has a provider or a transition in progress; nothing was started".into());
        }
    }
    if has_native_owner(source).map_err(|error| error.message)? {
        return Err("This conversation is still running, not attached; stop its earlier provider before resuming".into());
    }
    if let Some(listener) = &source.owned_codex_listener
        && owned_listener::served(&listener.socket).map_err(|error| error.to_string())?
    {
        return Err("This conversation is still running, not attached; stop its earlier provider before resuming".into());
    }
    if source
        .proof
        .as_ref()
        .is_some_and(|old| old.boot_id == proof.boot_id)
    {
        verify_resume_processes(host, source, &proof)?;
        if matches!(
            source.native_continuity,
            chat::NativeContinuity::RetainedClaudeActor { .. }
        ) {
            let retirement = retained_resume::inspect_resume_retirement(host, source)?;
            if !retained_resume::prepare_resume_verified(host, source, retirement.verified())? {
                return Err(
                    "The earlier Claude provider's end could not be proved; nothing was started"
                        .into(),
                );
            }
        }
    }
    super::super::agent_execution::stopped(host, source)?;
    let mut successor = source.clone();
    successor.reset_owned_codex_listener();
    successor.id = TerminalId::new();
    successor.proof = None;
    successor.terminal = None;
    if let Ok(registry) = super::super::agent_execution::registry(host) {
        registry.lock().unwrap_or_else(|p| p.into_inner()).succeed(
            successor.id,
            source.id,
            source
                .terminal
                .as_ref()
                .and_then(|terminal| terminal.recovery_origin),
            |members| {
                host.lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .any_other_live(members, source.id)
            },
        )?;
    }
    Ok(Some(successor))
}

fn verify_resume_processes(
    _host: &SharedHost,
    source: &Session,
    proof: &HostSessionProof,
) -> Result<(), String> {
    let recipe = source
        .launch
        .terminal_recipe()
        .ok_or("The recorded conversation has no exact resume recipe")?;
    let process_root = resume_process_root();
    #[cfg(target_os = "macos")]
    if source.launch.provider == Provider::Codex && process_root == Path::new("/proc") {
        if let Some(terminal) = &source.terminal {
            verify_darwin_paired_close(_host, source, terminal, proof)?;
        }
        require_ended_codex(source, proof)?;
        // Keep the exact issued pair in the descriptor used to match the
        // native holder's receipt. Terminal cleanup was checked separately.
        if !chat::native_holder::inspect_closed_source(source, &proof.boot_id)? {
            return Err("The earlier Codex provider's exact close receipt is unavailable; nothing was started".into());
        }
        return Ok(());
    }
    verify_procfs_resume(source, proof, recipe, &process_root)
}

#[cfg(target_os = "macos")]
fn verify_darwin_paired_close(
    host: &SharedHost,
    source: &Session,
    terminal: &crate::model::TerminalSession,
    proof: &HostSessionProof,
) -> Result<(), String> {
    if host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .is_live(&terminal.id)
    {
        return Err("This conversation still has a running console; nothing was started".into());
    }
    let (owner, _) = chat::native_holder::closed_source(source, proof)?
        .filter(|(_, paired_closed)| *paired_closed)
        .ok_or("The paired console has no confirmed close receipt")?;
    let pair = chat::native_holder::closed_pair_holder(source, proof, &owner, false)?
        .ok_or("The native close receipt does not identify its paired console")?;
    let issuer = source
        .proof
        .as_ref()
        .ok_or("The paired console has no issuing daemon proof")?;
    let role = crate::daemon::holder_registry::OriginalRole::authenticated(&pair, issuer, proof)?;
    if !crate::terminal::holder::cleanup::confirmed_with_role(
        terminal,
        &pair,
        proof,
        role.as_ref(),
    )? {
        return Err("The earlier console's end could not be proved; nothing was started".into());
    }
    Ok(())
}

fn require_ended_codex(source: &Session, proof: &HostSessionProof) -> Result<(), String> {
    if !super::super::upgrade_inspect::codex_execution_ended(source, &proof.boot_id) {
        return Err(
            "The earlier Codex provider's end could not be proved; nothing was started".into(),
        );
    }
    Ok(())
}

fn verify_procfs_resume(
    source: &Session,
    proof: &HostSessionProof,
    recipe: crate::model::LaunchRecipe,
    process_root: &Path,
) -> Result<(), String> {
    let terminal = crate::model::TerminalSession {
        id: source.id,
        launch: recipe,
        owned_codex_listener: source.owned_codex_listener.clone(),
        recovery_origin: None,
        predecessor: None,
        transcript_path: None,
        resume_hint: None,
        continuity: crate::model::TerminalContinuity::Live {
            host_session_id: proof.host_session_id.clone(),
            boot_id: proof.boot_id.clone(),
            host_incarnation: proof.host_incarnation.clone(),
        },
    };
    if crate::upgrade::surviving_process(&terminal, std::process::id(), process_root, &[])
        .map_err(|error| error.to_string())?
        .is_some()
    {
        return Err("This conversation is still running elsewhere; nothing was started".into());
    }
    if source.launch.provider == Provider::Codex {
        let mut native = source.clone();
        // A paired terminal has its own lifetime; prove it gone before
        // using the standard native-listener/provider census.
        if let Some(terminal) = &source.terminal {
            if crate::upgrade::surviving_process(terminal, std::process::id(), process_root, &[])
                .map_err(|error| error.to_string())?
                .is_some()
            {
                return Err(
                    "This conversation still has a running console; nothing was started".into(),
                );
            }
            native.terminal = None;
        }
        require_ended_codex(&native, proof)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_floor_reader_keeps_transcript_without_executing_the_launch_recipe() {
        let root = tempfile::tempdir().unwrap();
        let proof = |name| HostSessionProof {
            host_session_id: crate::model::HostSessionId::new("floor-reader"),
            boot_id: crate::model::BootId::new("boot"),
            host_incarnation: crate::model::HostIncarnation::new(name),
        };
        let host = Arc::new(HostLock::new(HostState::new(proof("new"))));
        let source = Session {
            id: TerminalId::new(),
            launch: chat::Launch {
                provider: Provider::Codex,
                recipe: crate::model::LaunchRecipe {
                    program: "/must-not-execute-floor-recipe".into(),
                    arguments: vec![],
                    working_directory: root.path().to_owned(),
                    environment: Default::default(),
                    unset_environment: Default::default(),
                },
                session_id: Some(uuid::Uuid::new_v4().to_string()),
                draft_model: None,
            },
            proof: Some(proof("old")),
            terminal: None,
            owned_codex_listener: None,
            native_continuity: chat::NativeContinuity::Durable {
                provider_version: "1.0".into(),
            },
        };
        let saved = crate::upgrade::floor::StoppedSnapshot {
            source: source.clone(),
            limitation: None,
            snapshot: chat::Snapshot {
                session_id: source.launch.session_id.clone(),
                title: Some("kept transcript".into()),
                ..chat::Snapshot::default()
            },
        };
        install(&host, &saved, &source).unwrap();
        let handle = host
            .lock()
            .unwrap()
            .chats
            .get(&source.id)
            .unwrap()
            .handle
            .clone();
        // The reader applies its frame's read-only reason before its exit,
        // so wait for the exit, which ends the frame (#2964).
        let snapshot =
            crate::test_timing::wait_for("the stopped reader to apply its saved frame", || {
                handle.snapshot(None).filter(|snapshot| snapshot.exited)
            });
        assert!(snapshot.read_only_reason.is_some(), "{:?}", snapshot.error);
        assert_eq!(snapshot.title.as_deref(), Some("kept transcript"));
        assert_eq!(snapshot.session_id, source.launch.session_id);
        assert!(handle.is_read_only_history());
        assert!(!handle.owns_work());
        assert_eq!(
            handle.native_continuity(),
            chat::NativeContinuity::ReconnectOnly
        );
        handle.close().unwrap();
    }
}

#[cfg(test)]
mod flow_tests {
    use super::*;
    use crate::model::{CardId, CardKind};
    use crate::workspace_model::{
        ModelEdit,
        views::{Change, Pane, Scope},
    };

    fn provider_history_ignores_unreadable_floor(root: &Path, proof: HostSessionProof) {
        let cwd = root.join("missing-project");
        let config = root.join("claude-fixture");
        let thread = uuid::Uuid::new_v4().to_string();
        let project: String = cwd
            .to_str()
            .unwrap()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        let project = config.join("projects").join(project);
        std::fs::create_dir_all(&project).unwrap();
        let item = serde_json::json!({
            "type": "user", "uuid": "kept-message", "parentUuid": null, "sessionId": thread,
            "message": { "role": "user", "content": "Ordinary provider history" }
        });
        std::fs::write(project.join(format!("{thread}.jsonl")), format!("{item}\n")).unwrap();
        let source = Session {
            id: TerminalId::new(),
            launch: chat::Launch {
                provider: Provider::Claude,
                recipe: crate::model::LaunchRecipe {
                    program: "/must-not-execute-provider-recipe".into(),
                    arguments: vec![],
                    working_directory: cwd,
                    environment: BTreeMap::from([(
                        "CLAUDE_CONFIG_DIR".into(),
                        config.to_str().unwrap().into(),
                    )]),
                    unset_environment: Default::default(),
                },
                session_id: Some(thread),
                draft_model: None,
            },
            proof: Some(proof.clone()),
            terminal: None,
            owned_codex_listener: None,
            native_continuity: chat::NativeContinuity::ReconnectOnly,
        };
        let mut state = HostState::new(proof);
        state.install_model(
            crate::host::model::Registry::at(&root.join("provider-history.sqlite3")).unwrap(),
        );
        state.register_client(1);
        let host = Arc::new(HostLock::new(state));
        let id = source.id;
        let response = crate::host::client::process_request(
            &host,
            1,
            ClientRequest::ChatAttach { session: source },
        )
        .unwrap()
        .0;
        assert!(matches!(response, ServerResponse::ChatAttached { .. }));
        let handle = host.lock().unwrap().chats[&id].handle.clone();
        let snapshot =
            crate::test_timing::wait_for("the provider history reader to settle", || {
                handle.snapshot(None).filter(|snapshot| snapshot.exited)
            });
        assert!(snapshot.items.iter().any(|item| matches!(&item.content,
            chat::ItemContent::Text { text, .. } if text == "Ordinary provider history"
        )));
        handle.close().unwrap();
    }

    fn verify_history_retirement(host: &SharedHost, original: &Session) {
        use crate::host::client::process_request;
        let id = original.id;
        let request = |client, request| process_request(host, client, request).unwrap().0;
        let resolve = |client| {
            let ServerResponse::UpgradeChatResolved { session } = request(
                client,
                ClientRequest::ResolveUpgradeChat {
                    source: original.clone(),
                },
            ) else {
                panic!("resolve did not return the saved history");
            };
            session
        };
        let resolved = resolve(1);
        let handle = host.lock().unwrap().chats[&id].handle.clone();
        crate::test_timing::wait_for("the resolved history reader to settle", || {
            handle.snapshot(None).filter(|snapshot| snapshot.exited)
        });
        let collect = || crate::host::chat::history_retirement::collect(host).unwrap();
        collect();
        assert!(
            host.lock().unwrap().chats.contains_key(&id),
            "A pending Resolve pins its reader"
        );
        upgrade_resolution::expire_for_test(host, 1);
        collect();
        {
            let state = host.lock().unwrap();
            assert!(state.clients.contains_key(&1));
            assert!(
                !state.chats.contains_key(&id),
                "an abandoned Resolve expires"
            );
        }
        assert!(handle.is_completed());

        // A delayed Attach can reopen the durable source, even if the original
        // reader has already been released.
        request(
            1,
            ClientRequest::ChatAttach {
                session: resolved.clone(),
            },
        );
        let handle = host.lock().unwrap().chats[&id].handle.clone();
        host.lock().unwrap().register_client(2);
        request(
            2,
            ClientRequest::ChatAttach {
                session: resolved.clone(),
            },
        );
        request(1, ClientRequest::ChatDetach { chat_id: id });
        collect();
        assert!(
            host.lock().unwrap().chats.contains_key(&id),
            "a hidden subscriber still pins its reader"
        );

        let repository = original.launch.recipe.working_directory.clone();
        let scope = Scope::Worktree {
            repository: repository.clone(),
            path: repository,
        };
        let pane = Pane {
            id: CardId::new(),
            title: "Paused history".into(),
            user_title: None,
            pending_terminal_start: false,
            width_fraction: 1.0,
            kind: CardKind::Chat(resolved.clone()),
            scope: None,
        };
        let edit = |edit| request(1, ClientRequest::ApplyWorkspaceEdit { edit });
        edit(ModelEdit::OpenProject {
            repository: original.launch.recipe.working_directory.clone(),
            name: "Project".into(),
            kind: Default::default(),
        });
        let ServerResponse::WorkspaceEdit(seeded) = edit(ModelEdit::SeedView {
            scope: scope.clone(),
            panes: vec![pane.clone()],
            closed: vec![],
        }) else {
            panic!("seed did not commit");
        };
        host.lock().unwrap().unregister_client(2);
        collect();
        assert!(
            host.lock().unwrap().chats.contains_key(&id),
            "the saved pane alone pins its reader"
        );

        crate::host::chat::history_retirement::start(host).unwrap();
        edit(ModelEdit::ChangeView {
            scope,
            expected_revision: seeded.effects.views[0].after.unwrap(),
            operation: uuid::Uuid::new_v4(),
            change: Change::Remove { id: pane.id },
        });
        let wait_for_release = || {
            crate::test_timing::wait_for("the unreferenced reader to retire", || {
                (!host.lock().unwrap().chats.contains_key(&id)).then_some(())
            });
            assert!(crate::upgrade::floor::snapshot(original).is_ok());
        };
        wait_for_release();
        assert!(handle.is_completed());
        assert!(!host.lock().unwrap().upgrade_chats.contains_key(&id));

        // A previously issued current descriptor can still reopen the exact
        // paused page after automatic release.
        request(1, ClientRequest::ChatAttach { session: resolved });
        request(1, ClientRequest::ChatDetach { chat_id: id });
        wait_for_release();

        // A watcher may resolve and then go away without ever attaching.
        let release_initial =
            chat::runtime::hold_history_reader(&original.launch.recipe.working_directory);
        resolve(1);
        host.lock().unwrap().unregister_client(1);
        drop(release_initial);
        wait_for_release();
    }

    #[test]
    fn recorded_floor_resolves_stopped_and_explicit_resume_requires_a_new_quiet_allocation() {
        const ROOT: &str = "CAGENTS_FLOOR_HISTORY_FLOW_ROOT";
        let root = if let Some(root) = std::env::var_os(ROOT) {
            std::path::PathBuf::from(root)
        } else {
            let root = tempfile::tempdir().unwrap();
            for directory in ["home", "run", "data", "config", "cache", "project"] {
                std::fs::create_dir_all(root.path().join(directory)).unwrap();
            }
            let selector = thread::current().name().unwrap().to_owned();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", &selector, "--nocapture", "--test-threads=1"])
                .env(ROOT, root.path())
                .env("HOME", root.path().join("home"))
                .env("XDG_RUNTIME_DIR", root.path().join("run"))
                .env("XDG_DATA_HOME", root.path().join("data"))
                .env("XDG_CONFIG_HOME", root.path().join("config"))
                .env("XDG_CACHE_HOME", root.path().join("cache"))
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        };
        let processes = root.join("proc");
        std::fs::create_dir(&processes).unwrap();
        let _process_root = ProcessRootGuard::install(&processes);
        let proof = |name, boot| HostSessionProof {
            host_session_id: crate::model::HostSessionId::new("floor-flow"),
            boot_id: crate::model::BootId::new(boot),
            host_incarnation: crate::model::HostIncarnation::new(name),
        };
        let boot = crate::daemon::read_boot_id().unwrap();
        let old = proof("old", boot.as_str());
        let new = proof("new", boot.as_str());
        let mut record = crate::upgrade::floor::Record::new(old.clone(), vec![], vec![]);
        for (provider, recorded) in [Provider::Claude, Provider::Codex, Provider::Omp]
            .into_iter()
            .flat_map(|provider| [(provider, true), (provider, false)])
        {
            let source = Session {
                id: TerminalId::new(),
                launch: chat::Launch {
                    provider,
                    recipe: crate::model::LaunchRecipe {
                        program: provider.executable().into(),
                        arguments: vec![],
                        working_directory: root.join("project"),
                        environment: Default::default(),
                        unset_environment: Default::default(),
                    },
                    session_id: recorded.then(|| uuid::Uuid::new_v4().to_string()),
                    draft_model: None,
                },
                proof: Some(old.clone()),
                terminal: None,
                owned_codex_listener: None,
                native_continuity: chat::NativeContinuity::ReconnectOnly,
            };
            let mut source = source;
            if provider == Provider::Codex {
                let socket = root
                    .join(format!("cagents-codex-{}", source.id))
                    .join("control.sock");
                source.launch.recipe.arguments =
                    vec!["--remote".into(), format!("unix://{}", socket.display())];
                source.owned_codex_listener = Some(chat::OwnedCodexListener {
                    socket,
                    allocator: old.clone(),
                });
            }
            record
                .snapshots
                .push(crate::upgrade::floor::StoppedSnapshot {
                    source: source.clone(),
                    snapshot: chat::Snapshot {
                        session_id: source.launch.session_id.clone(),
                        title: Some("floor transcript".into()),
                        items: vec![Arc::new(chat::Item {
                            id: "kept-message".into(),
                            role: chat::Role::Assistant,
                            content: chat::ItemContent::Text {
                                text: "Final source answer remains visible".into(),
                                thinking: false,
                                phase: Default::default(),
                            },
                            started_at: None,
                            at: None,
                        })],
                        ..Default::default()
                    },
                    limitation: None,
                });
            record.chats.push(source);
        }
        let mut stale = record.chats[0].clone();
        stale.launch.session_id = None;
        record.aliases.push(stale.clone());
        // Retained adoption can consult this index before Prepared recovery
        // publishes its paused record in the same daemon process.
        let before_publication = crate::upgrade::floor::display_snapshot();
        assert!(before_publication.resume_source(&stale).is_none());
        record.save().unwrap();
        assert!(before_publication.resume_source(&stale).is_none());
        assert!(
            crate::upgrade::floor::display_snapshot()
                .resume_source(&stale)
                .is_some()
        );
        let published = crate::upgrade::floor::display_snapshot();
        let invalid = crate::instance::project_dirs()
            .unwrap()
            .data_local_dir()
            .join("daemon-upgrade/update-floor/invalid.json");
        std::fs::write(&invalid, b"{").unwrap();
        assert!(record.save().is_err());
        assert!(Arc::ptr_eq(
            &published,
            &crate::upgrade::floor::display_snapshot()
        ));
        provider_history_ignores_unreadable_floor(&root, new.clone());
        std::fs::remove_file(&invalid).unwrap();
        let path = std::fs::read_dir(invalid.parent().unwrap())
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let held = path.with_extension("held");
        std::fs::rename(&path, &held).unwrap();
        assert!(
            crate::upgrade::floor::load()
                .unwrap()
                .resume_source(&stale)
                .is_none()
        );
        assert!(
            crate::upgrade::floor::display_snapshot()
                .resume_source(&stale)
                .is_none()
        );
        assert!(published.resume_source(&stale).is_some());
        std::fs::rename(held, path).unwrap();
        assert!(
            crate::upgrade::floor::load()
                .unwrap()
                .resume_source(&stale)
                .is_some()
        );
        let mut state = HostState::new(new.clone());
        state.install_model(
            crate::host::model::Registry::at(&root.join("registry.sqlite3")).unwrap(),
        );
        state.register_client(1);
        let host = Arc::new(HostLock::new(state));
        for original in &record.chats {
            let saved = if original.id == stale.id {
                &stale
            } else {
                original
            };
            let resolved = super::super::upgrade_resolution::resolve(&host, 1, saved).unwrap();
            assert_eq!(resolved.launch.session_id, original.launch.session_id);
            assert!(
                resolved.is_saved_claude_history(),
                "all floor providers use the existing stopped history controls"
            );
            assert!(chat::is_upgrade_recovery(saved, &resolved));
            let ServerResponse::ChatAttached {
                session: reader, ..
            } = super::super::process(&host, 1, ClientRequest::ChatAttach { session: resolved })
                .unwrap()
            else {
                panic!("no stopped attachment")
            };
            let handle = host
                .lock()
                .unwrap()
                .chats
                .get(&original.id)
                .unwrap()
                .handle
                .clone();
            // The reader applies its saved frame after attach returns; its exit
            // ends that frame.
            let snapshot =
                crate::test_timing::wait_for("the stopped reader to apply its saved frame", || {
                    handle.snapshot(None).filter(|snapshot| snapshot.exited)
                });
            assert!(!handle.owns_work());
            assert!(snapshot.items.iter().any(|item|matches!(&item.content,chat::ItemContent::Text {text,..} if text == "Final source answer remains visible")));
            if original.launch.session_id.is_none() {
                assert!(
                    snapshot
                        .read_only_reason
                        .as_ref()
                        .unwrap()
                        .contains("start a new conversation")
                );
                assert!(
                    super::super::process(
                        &host,
                        1,
                        ClientRequest::ChatPrepareResume { source: reader }
                    )
                    .unwrap_err()
                    .message
                    .contains("no recorded conversation")
                );
                continue;
            }
            let prepare = |host: &SharedHost| {
                super::super::process(
                    host,
                    1,
                    ClientRequest::ChatPrepareResume {
                        source: original.clone(),
                    },
                )
            };
            let prepared = {
                #[cfg(target_os = "macos")]
                let _native = (original.launch.provider == Provider::Codex)
                    .then(|| ProcessRootGuard::install(Path::new("/proc")));
                prepare(&host)
            };
            #[cfg(target_os = "macos")]
            let prepared = if original.launch.provider == Provider::Codex {
                // This stopped ledger has no exact Codex retirement receipt.
                // It remains readable, but on this boot it grants no writer.
                let failure = prepared.unwrap_err();
                assert_eq!(failure.code, ProtocolErrorCode::TerminalUnavailable);
                assert!(
                    failure.message.starts_with("The earlier Codex provider's")
                        && failure.message.contains("nothing was started"),
                    "{failure:?}"
                );
                {
                    let state = host.lock().unwrap();
                    assert!(state.chat_spawning.is_empty());
                    assert!(state.chats[&original.id].handle.is_read_only_history());
                }
                // After a reboot the old provider cannot survive. Exercise
                // the same ledger's positive resume with a successor host.
                let mut restarted = HostState::new(proof("rebooted", "next-boot"));
                restarted.register_client(1);
                restarted.install_model(
                    crate::host::model::Registry::at(&root.join("rebooted.sqlite3")).unwrap(),
                );
                prepare(&Arc::new(HostLock::new(restarted)))
            } else {
                prepared
            };
            let ServerResponse::ChatResumePrepared { session: successor } = prepared.unwrap()
            else {
                panic!("no resume preparation")
            };
            assert_ne!(successor.id, original.id);
            let mut expected = original.clone();
            expected.reset_owned_codex_listener();
            assert_eq!(successor.launch, expected.launch);
            assert!(successor.proof.is_none());
            assert!(chat::is_retained_resume_successor(&reader, &successor));
        }
        // A process outside this host still names the old exact conversation.
        // This is parsed by the production census against an owned proc fixture,
        // avoiding unrelated protected processes on a shared build machine.
        let source = &record.chats[0];
        let pid = std::process::id() + 1;
        let process = processes.join(pid.to_string());
        std::fs::create_dir(&process).unwrap();
        let mut fields = vec!["0".to_owned(); 52];
        fields[0] = "S".into();
        fields[1] = "1".into();
        fields[2] = pid.to_string();
        fields[3] = pid.to_string();
        fields[19] = "700".into();
        std::fs::write(
            process.join("stat"),
            format!("{pid} (old-provider) {}", fields.join(" ")),
        )
        .unwrap();
        std::fs::write(
            process.join("cmdline"),
            format!(
                "claude\0--resume\0{}\0",
                source.launch.session_id.as_ref().unwrap()
            ),
        )
        .unwrap();
        std::fs::write(process.join("environ"), b"PATH=/usr/bin\0").unwrap();
        let failure = super::super::process(
            &host,
            1,
            ClientRequest::ChatPrepareResume {
                source: source.clone(),
            },
        )
        .unwrap_err();
        assert!(
            failure.message.contains("still running elsewhere"),
            "{}",
            failure.message
        );
        std::fs::remove_dir_all(&process).unwrap();
        // A different allocation already writing this conversation vetoes only
        // its explicit Resume, even after a confirmed service stop.
        let source = &record.chats[0];
        let mut live = source.clone();
        live.id = TerminalId::new();
        live.proof = Some(new);
        let mut spec = ProcessSpec::from(&live.launch.recipe);
        spec.program = "/bin/cat".into();
        spec.arguments.clear();
        spec.transport = TransportSpec::JsonLines;
        let mut initial = Step::default();
        let store = chat::runtime::preflight(&spec, &mut initial).unwrap();
        let live_handle = Handle::start_owned(
            spec,
            Box::new(StoppedAdapter),
            initial,
            store,
            live.clone(),
            Default::default(),
            None,
            |_| {},
        )
        .unwrap();
        assert!(live_handle.owns_work());
        host.lock().unwrap().chats.insert(
            live.id,
            Record {
                session: live,
                handle: live_handle.clone(),
                execution_instance: uuid::Uuid::new_v4(),
                console_recipe: None,
                paired_terminal: None,
                source_process: None,
                subscribers: BTreeMap::new(),
                observations: BTreeMap::new(),
                completion_observed: false,
                returning_terminal: false,
            },
        );
        assert!(
            prepare_resume(&host, source)
                .unwrap_err()
                .contains("still has a provider")
        );
        live_handle.close().unwrap();
        let handles: Vec<_> = host
            .lock()
            .unwrap()
            .chats
            .values()
            .map(|record| record.handle.clone())
            .collect();
        for handle in handles {
            handle.close().unwrap();
        }
        let mut state = HostState::new(proof("retirement", boot.as_str()));
        state.install_model(
            crate::host::model::Registry::at(&root.join("retirement.sqlite3")).unwrap(),
        );
        state.register_client(1);
        let host = Arc::new(HostLock::new(state));
        let unnamed_codex = record
            .chats
            .iter()
            .find(|source| {
                source.launch.provider == Provider::Codex && source.launch.session_id.is_none()
            })
            .unwrap();
        verify_history_retirement(&host, unnamed_codex);
    }
}
