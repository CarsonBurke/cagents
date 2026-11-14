//! A stopped update pane pages the reviewed transcript without a provider.
use super::*;
use crate::chat::protocol::{Adapter, ProcessSpec, Step, TransportSpec};

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
        if host
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .chats
            .contains_key(&id)
        {
            return Ok(());
        }
        let mut session = saved.source.clone();
        session.proof = Some(proof.clone());
        session.reset_owned_codex_listener();
        session.native_continuity = chat::NativeContinuity::ReconnectOnly;
        session.terminal = None;
        session.owned_codex_listener = None;
        let mut spec = ProcessSpec::from(&session.launch.recipe);
        spec.transport = TransportSpec::History;
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
        let environment = spec.environment.clone();
        let weak = Arc::downgrade(host);
        let handle = Handle::start_owned(
            spec,
            Box::new(StoppedAdapter),
            initial,
            store,
            session.clone(),
            environment,
            move |update| changed(&weak, id, update),
        )?;
        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
        state
            .upgrade_chats
            .entry(id)
            .or_default()
            .push(crate::upgrade::handoff::ChatSource {
                source: saved.source.clone(),
                launch: saved.source.launch.clone(),
            });
        if requested != &saved.source {
            state
                .upgrade_chats
                .entry(id)
                .or_default()
                .push(crate::upgrade::handoff::ChatSource {
                    source: requested.clone(),
                    launch: requested.launch.clone(),
                });
        }
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
        state.inventory_generation += 1;
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
        let recipe = source
            .launch
            .terminal_recipe()
            .ok_or("The recorded conversation has no exact resume recipe")?;
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
        if crate::upgrade::surviving_process(
            &terminal,
            std::process::id(),
            &resume_process_root(),
            &[],
        )
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
                if crate::upgrade::surviving_process(
                    terminal,
                    std::process::id(),
                    &resume_process_root(),
                    &[],
                )
                .map_err(|error| error.to_string())?
                .is_some()
                {
                    return Err(
                        "This conversation still has a running console; nothing was started".into(),
                    );
                }
                native.terminal = None;
            }
            if !super::super::upgrade_inspect::codex_execution_ended(&native, &proof.boot_id) {
                return Err(
                    "The earlier Codex provider's end could not be proved; nothing was started"
                        .into(),
                );
            }
        }
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
        let deadline = Instant::now() + Duration::from_secs(5);
        let snapshot = loop {
            let snapshot = handle.snapshot(None).unwrap();
            if snapshot.read_only_reason.is_some() {
                break snapshot;
            }
            assert!(
                Instant::now() < deadline,
                "stopped reader did not initialize"
            );
            std::thread::sleep(Duration::from_millis(5));
        };
        assert!(snapshot.exited);
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
            let deadline = Instant::now() + Duration::from_secs(5);
            while !handle.snapshot(None).unwrap().exited {
                assert!(Instant::now() < deadline);
                thread::sleep(Duration::from_millis(5));
            }
            assert!(!handle.owns_work());
            let snapshot = handle.snapshot(None).unwrap();
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
            let ServerResponse::ChatResumePrepared { session: successor } = super::super::process(
                &host,
                1,
                ClientRequest::ChatPrepareResume {
                    source: original.clone(),
                },
            )
            .unwrap() else {
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
    }
}
