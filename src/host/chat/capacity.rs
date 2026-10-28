//! Resident actors and in-flight starts share one bounded admission budget.
use super::*;

pub(in crate::host) fn usage(state: &HostState) -> usize {
    state.chats.len()
        + state
            .chat_spawning
            .keys()
            .filter(|id| !state.chats.contains_key(id))
            .count()
}

pub(super) fn require_available(state: &HostState) -> Result<(), ProtocolError> {
    let used = usage(state);
    if used >= state.native_chat_capacity {
        return Err(ProtocolError::new(
            ProtocolErrorCode::ChatCapacity,
            format!(
                "Daemon native chat capacity reached: {} resident, {} starting, limit {} ({} distinct slots used). Release explicitly verified completed chats or change the configured capacity; idle chats may still own provider work.",
                state.chats.len(),
                state.chat_spawning.len(),
                state.native_chat_capacity,
                used,
            ),
            true,
        ));
    }
    Ok(())
}

/// An exact descriptor is the reservation token. Other transitions cannot
/// replace it while it is present; cleanup never removes a successor token.
pub(super) struct Reservation {
    host: SharedHost,
    source: Session,
}

impl Reservation {
    /// Fence teardown through durable retirement, including the interval after
    /// its resident record is removed and before its old authority is retired.
    pub(super) fn closing(
        host: &SharedHost,
        state: &mut HostState,
        source: &Session,
    ) -> Result<Self, ProtocolError> {
        if state.chat_spawning.contains_key(&source.id) {
            return Err(error("Chat is already starting or closing"));
        }
        state.chat_spawning.insert(source.id, source.clone());
        state.inventory_generation += 1;
        Ok(Self {
            host: host.clone(),
            source: source.clone(),
        })
    }

    pub(super) fn holds(&self, state: &HostState, id: TerminalId) -> bool {
        self.source.id == id && state.chat_spawning.get(&id) == Some(&self.source)
    }

    pub(super) fn new(host: &SharedHost, source: &Session) -> Result<Option<Self>, ProtocolError> {
        Self::new_with_source(host, source, None)
    }

    pub(super) fn new_with_source(
        host: &SharedHost,
        source: &Session,
        terminal_proof: Option<&HostSessionProof>,
    ) -> Result<Option<Self>, ProtocolError> {
        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
        if state.shutting_down {
            return Err(unavailable("Daemon is shutting down"));
        }
        if state.chat_spawning.contains_key(&source.id)
            || state.native_closing.contains_key(&source.id)
        {
            return Err(error("Chat is already starting or closing"));
        }
        if let Some(record) = state.chats.get(&source.id) {
            // An exact live Codex terminal pair is an attachment, not admission.
            // Authenticate it before allowing the existing fast path to run.
            let paired = (terminal_proof.is_some() || source.owns_codex_listener())
                && record.handle.owns_work()
                && source.launch.provider == Provider::Codex
                && record.session.launch.provider == Provider::Codex
                && record.handle.session_id() == source.launch.session_id
                && record.session.terminal.as_ref().is_some_and(|terminal| {
                    source.terminal.as_ref().is_some_and(|source| {
                        terminal.id == source.id && terminal.launch == source.launch
                    }) && state
                        .terminals
                        .get(&terminal.id)
                        .is_some_and(|live| live.session.launch == terminal.launch)
                });
            // Older source-based callers may omit terminal metadata. The
            // second attachment path resolves their exact proven live worker
            // before checking the conversation; preserve that path without
            // treating an arbitrary live chat as recoverable.
            let resolving_pair = terminal_proof.is_some()
                && source.launch.provider == Provider::Codex
                && record.session.launch.provider == Provider::Codex
                && record.session.terminal.as_ref().is_some_and(|terminal| {
                    terminal.id == source.id
                        && state.terminals.get(&terminal.id).is_some_and(|live| {
                            live.session.launch == terminal.launch
                                && record
                                    .paired_terminal
                                    .as_ref()
                                    .is_some_and(|paired| paired.same_worker(&live.handle))
                        })
                });
            if paired || resolving_pair {
                validate_proof(terminal_proof.or(source.proof.as_ref()), state.proof())?;
                return Ok(None);
            }
            if record.returning_terminal || record.handle.owns_work() {
                return Err(error(
                    "Chat already owns work or a pending handoff; attach to its existing view",
                ));
            }
            validate_proof(source.proof.as_ref(), state.proof())?;
            if record.session.launch.provider != source.launch.provider
                || record.session.launch.recipe != source.launch.recipe
                || record.handle.session_id() != source.launch.session_id
            {
                return Err(error(
                    "Explicit recovery must retain the exact closed chat identity",
                ));
            }
        }
        if state.chat_handoffs.contains_key(&source.id) {
            return Err(error(
                "This chat already returned to terminal; complete its handoff",
            ));
        }
        if state.chats.values().any(|record| {
            let mut launch = record.session.launch.clone();
            launch.session_id = record.handle.session_id();
            record.handle.owns_work() && same_conversation(&launch, &source.launch)
        }) {
            return Err(error(
                "This exact conversation already owns work; attach to its existing chat",
            ));
        }
        if state
            .chat_spawning
            .values()
            .any(|pending| same_conversation(&pending.launch, &source.launch))
        {
            return Err(error("This exact conversation is already starting"));
        }
        if !state.chats.contains_key(&source.id) {
            require_available(&state)?;
        }
        state.chat_spawning.insert(source.id, source.clone());
        state.inventory_generation += 1;
        Ok(Some(Self {
            host: host.clone(),
            source: source.clone(),
        }))
    }

    pub(super) fn install(
        admission: &mut Option<Self>,
        host: &SharedHost,
        state: &mut HostState,
        source: &Session,
    ) -> Result<(), ProtocolError> {
        if let Some(admission) = admission.as_ref()
            && state.chat_spawning.get(&admission.source.id) != Some(&admission.source)
        {
            return Err(error("Native chat admission reservation changed"));
        }
        state.chat_spawning.insert(source.id, source.clone());
        if let Some(admission) = admission {
            admission.source = source.clone();
        } else {
            *admission = Some(Self {
                host: host.clone(),
                source: source.clone(),
            });
        }
        Ok(())
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        let mut state = self.host.lock().unwrap_or_else(|p| p.into_inner());
        if state.chat_spawning.get(&self.source.id) == Some(&self.source) {
            state.chat_spawning.remove(&self.source.id);
            state.inventory_generation += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{BootId, HostIncarnation, HostSessionId};

    fn host() -> SharedHost {
        Arc::new(HostLock::new(HostState::new(HostSessionProof {
            host_session_id: HostSessionId::new("capacity-test"),
            boot_id: BootId::new("boot"),
            host_incarnation: HostIncarnation::new("incarnation"),
        })))
    }

    fn session() -> Session {
        Session {
            id: TerminalId::new(),
            launch: chat::Launch {
                provider: Provider::Claude,
                session_id: None,
                recipe: crate::model::LaunchRecipe {
                    program: "/missing-provider-never-constructed".into(),
                    arguments: Vec::new(),
                    working_directory: "/missing-capacity-test-project".into(),
                    environment: Default::default(),
                    unset_environment: Default::default(),
                },
            },
            proof: None,
            terminal: None,
            owned_codex_listener: None,
            native_continuity: Default::default(),
        }
    }

    #[test]
    fn rejected_start_precedes_account_setup_and_provider_history_reads() {
        let host = host();
        {
            let mut state = host.lock().unwrap();
            state.native_chat_capacity = 1;
            let occupied = session();
            state.chat_spawning.insert(occupied.id, occupied);
        }
        // This launch cannot pass account/history/adapter setup. Capacity must
        // be the first failure, before any attempt to create its environment.
        let mut rejected = session();
        rejected.launch.session_id = Some("12345678-1234-4234-8234-123456789abc".into());
        let failure = start(&host, 1, rejected, None, false, None, Default::default()).unwrap_err();
        assert_eq!(failure.code, ProtocolErrorCode::ChatCapacity);
        assert!(failure.message.contains("0 resident, 1 starting, limit 1"));
        assert_eq!(host.lock().unwrap().chat_spawning.len(), 1);
    }

    #[test]
    fn last_slot_is_reserved_atomically_until_its_owner_finishes() {
        let host = host();
        host.lock().unwrap().native_chat_capacity = 1;
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let (ready, results) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel();
        let released = Arc::new(std::sync::Mutex::new(released));
        let workers: Vec<_> = (0..2)
            .map(|_| {
                let host = host.clone();
                let barrier = barrier.clone();
                let ready = ready.clone();
                let released = released.clone();
                thread::spawn(move || {
                    barrier.wait();
                    let reservation = Reservation::new(&host, &session());
                    ready.send(reservation.is_ok()).unwrap();
                    if reservation.is_ok() {
                        released.lock().unwrap().recv().unwrap();
                    }
                    drop(reservation);
                })
            })
            .collect();
        barrier.wait();
        let admitted = (0..2)
            .map(|_| results.recv_timeout(Duration::from_secs(2)).unwrap())
            .filter(|admitted| *admitted)
            .count();
        release.send(()).unwrap();
        for worker in workers {
            worker.join().unwrap();
        }
        assert_eq!(admitted, 1);
        assert_eq!(usage(&host.lock().unwrap()), 0);
    }

    #[test]
    fn failed_start_and_stale_guard_do_not_leak_or_remove_successor_reservations() {
        let host = host();
        let original = session();
        let admission = Reservation::new(&host, &original).unwrap();
        assert!(Reservation::new(&host, &original).is_err());
        let mut successor = original.clone();
        successor.launch.recipe.program = "/successor-provider".into();
        host.lock()
            .unwrap()
            .chat_spawning
            .insert(original.id, successor.clone());
        drop(admission);
        assert_eq!(
            host.lock().unwrap().chat_spawning.get(&original.id),
            Some(&successor)
        );
        host.lock().unwrap().chat_spawning.clear();
        assert!(start(&host, 1, original, None, false, None, Default::default()).is_err());
        assert!(host.lock().unwrap().chat_spawning.is_empty());
    }

    #[test]
    fn pending_same_conversation_is_rejected_before_another_slot_is_consumed() {
        let host = host();
        let mut first = session();
        first.launch.session_id = Some("same-provider-thread".into());
        let reservation = Reservation::new(&host, &first).unwrap();
        let mut second = first.clone();
        second.id = TerminalId::new();
        assert!(Reservation::new(&host, &second).is_err());
        assert_eq!(usage(&host.lock().unwrap()), 1);
        drop(reservation);
        assert_eq!(usage(&host.lock().unwrap()), 0);
    }
    fn closed_history(host: &SharedHost) -> (tempfile::TempDir, Session, Handle) {
        history_reader(host, true)
    }

    fn history_reader(host: &SharedHost, close: bool) -> (tempfile::TempDir, Session, Handle) {
        let directory = tempfile::tempdir().unwrap();
        let registry = super::super::super::model::Registry::at(
            &directory.path().join("capacity-model.sqlite"),
        )
        .unwrap();
        host.lock().unwrap().execution_registry = Some(Arc::new(std::sync::Mutex::new(registry)));
        let mut source = session();
        source.proof = Some(host.lock().unwrap().proof().clone());
        source.launch.session_id = Some("12345678-1234-4234-8234-123456789abc".into());
        source.launch.recipe.working_directory = directory.path().join("missing-project");
        source.launch.recipe.environment.insert(
            "CLAUDE_CONFIG_DIR".into(),
            directory.path().join("claude").to_str().unwrap().into(),
        );
        let project: String = source
            .launch
            .recipe
            .working_directory
            .to_str()
            .unwrap()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        let transcript = directory
            .path()
            .join("claude/projects")
            .join(project)
            .join(format!(
                "{}.jsonl",
                source.launch.session_id.as_ref().unwrap()
            ));
        std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        std::fs::write(&transcript, serde_json::json!({"type":"user","uuid":"saved-item","parentUuid":null,"sessionId":source.launch.session_id,"message":{"role":"user","content":"durable history body"}}).to_string()+"\n").unwrap();
        let (spec, mut adapter) = chat::protocol::create(&source.launch).unwrap();
        assert!(matches!(
            spec.transport,
            chat::protocol::TransportSpec::History
        ));
        let mut initial = adapter.initialize().unwrap();
        let store = chat::runtime::preflight(&spec, &mut initial).unwrap();
        let handle = Handle::start(
            spec,
            adapter,
            initial,
            store,
            source.launch.session_id.clone(),
            |_| {},
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while handle.snapshot(None).unwrap().read_only_reason.is_none() {
            assert!(
                Instant::now() < deadline,
                "history initialization did not complete"
            );
            thread::sleep(Duration::from_millis(10));
        }
        if close {
            handle.close().unwrap();
        }
        host.lock().unwrap().chats.insert(
            source.id,
            Record {
                execution_instance: uuid::Uuid::new_v4(),
                console_recipe: None,
                paired_terminal: None,
                session: source.clone(),
                handle: handle.clone(),
                source_process: None,
                subscribers: BTreeMap::new(),
                observations: BTreeMap::new(),
                completion_observed: true,
                returning_terminal: false,
            },
        );
        (directory, source, handle)
    }

    #[test]
    fn pending_recovery_rejects_close_without_stopping_retained_actor() {
        let host = host();
        let (_directory, source, handle) = history_reader(&host, false);
        host.lock()
            .unwrap()
            .chats
            .get_mut(&source.id)
            .unwrap()
            .subscribers
            .insert(7, false);
        let admission = Reservation::new(&host, &source).unwrap();
        let failure =
            process(&host, 7, ClientRequest::ChatClose { chat_id: source.id }).unwrap_err();
        assert!(failure.message.contains("starting or closing"));
        assert!(
            !handle.is_completed(),
            "recovery's retained reader must not be stopped"
        );
        assert!(
            host.lock().unwrap().chats[&source.id]
                .handle
                .same_actor(&handle)
        );
        assert!(host.lock().unwrap().chat_spawning.contains_key(&source.id));
        let failure = close_terminal_actor(&host, source.id, &handle).unwrap_err();
        assert!(failure.message.contains("starting or closing"));
        assert!(
            !handle.is_completed(),
            "terminal close must preserve the same recovery reader"
        );
        assert!(
            host.lock().unwrap().chats[&source.id]
                .handle
                .same_actor(&handle)
        );
        drop(admission);
        handle.close().unwrap();
    }

    #[test]
    fn close_fences_replacement_before_retirement_and_after_resident_removal() {
        let host = host();
        let (_directory, source, handle) = history_reader(&host, false);
        host.lock()
            .unwrap()
            .chats
            .get_mut(&source.id)
            .unwrap()
            .subscribers
            .insert(0, false);
        let registry = host.lock().unwrap().execution_registry.clone().unwrap();
        // Internal execution client 0 dispatches directly to the shared close
        // handler, avoiding the interactive ownership lookup's earlier lock.
        // Hold the real preflight boundary: close must reserve before waiting
        // on durable retirement, so replacement cannot run provider setup.
        let locked = registry.lock().unwrap();
        let closing_host = host.clone();
        let id = source.id;
        let close = thread::spawn(move || {
            process(&closing_host, 0, ClientRequest::ChatClose { chat_id: id })
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        while !host.lock().unwrap().chat_spawning.contains_key(&id) {
            assert!(
                Instant::now() < deadline,
                "close did not reserve before retirement preflight"
            );
            thread::sleep(Duration::from_millis(5));
        }
        assert!(Reservation::new(&host, &source).is_err());
        assert!(!handle.is_completed());
        drop(locked);
        assert!(matches!(
            close.join().unwrap().unwrap(),
            ServerResponse::ChatClosed { .. }
        ));
        assert!(!host.lock().unwrap().chats.contains_key(&id));
        assert!(host.lock().unwrap().chat_spawning.is_empty());

        // The same lease must survive the final resident-removal boundary.
        // Otherwise a new same-ID authority could be minted before retired().
        let lease = {
            let mut state = host.lock().unwrap();
            Reservation::closing(&host, &mut state, &source).unwrap()
        };
        let failure = start(
            &host,
            7,
            source.clone(),
            None,
            false,
            None,
            Default::default(),
        )
        .unwrap_err();
        assert!(failure.message.contains("starting or closing"));
        drop(lease);
        assert!(Reservation::new(&host, &source).is_ok());
    }

    #[test]
    fn recovery_reservation_counts_existing_resident_once_above_lowered_limit() {
        let host = host();
        let (_directory, source, _) = closed_history(&host);
        let mut admission = Reservation::new(&host, &source).unwrap();
        assert!(admission.is_some());
        {
            let mut state = host.lock().unwrap();
            state.native_chat_capacity = 1;
            Reservation::install(&mut admission, &host, &mut state, &source).unwrap();
            assert_eq!(usage(&state), 1);
            assert!(require_available(&state).is_err());
        }
        drop(admission);
        assert!(host.lock().unwrap().chats.contains_key(&source.id));
    }
    #[test]
    fn failed_exact_recovery_keeps_the_previous_resident_actor_at_capacity() {
        let host = host();
        let (_directory, source, handle) = closed_history(&host);
        // The saved directory now exists, so this exact descriptor attempts a
        // writer rather than a history reader. Its deliberately absent program
        // fails after reservation, without invoking an external provider.
        std::fs::create_dir(&source.launch.recipe.working_directory).unwrap();
        host.lock().unwrap().native_chat_capacity = 1;
        assert!(
            start(
                &host,
                1,
                source.clone(),
                None,
                false,
                None,
                Default::default()
            )
            .is_err()
        );
        let state = host.lock().unwrap();
        assert!(state.chats[&source.id].handle.same_actor(&handle));
        assert!(state.chat_spawning.is_empty());
        assert_eq!(usage(&state), 1);
    }
    #[test]
    fn release_dry_run_and_exact_nonce_preserve_history_and_successor() {
        let host = host();
        let (_directory, source, handle) = closed_history(&host);
        let inspection = inspect(&host, source.id).unwrap();
        assert_eq!(inspection.release_blocked, None);
        release(&host, &inspection.proof, true).unwrap();
        assert!(host.lock().unwrap().chats.contains_key(&source.id));
        let mut stale_generation = inspection.proof.clone();
        stale_generation.generation += 1;
        assert!(release(&host, &stale_generation, false).is_err());
        host.lock()
            .unwrap()
            .chats
            .get_mut(&source.id)
            .unwrap()
            .execution_instance = uuid::Uuid::new_v4();
        assert!(release(&host, &inspection.proof, false).is_err());
        let fresh = inspect(&host, source.id).unwrap();
        release(&host, &fresh.proof, false).unwrap();
        assert!(!host.lock().unwrap().chats.contains_key(&source.id));
        drop(handle);
        let (_, mut adapter) = chat::protocol::create(&fresh.proof.session.launch).unwrap();
        let loaded = adapter.initialize().unwrap();
        assert!(
            loaded.events.iter().any(
                |event| matches!(event, chat::Event::History { items, .. } if !items.is_empty())
            )
        );
    }

    #[test]
    fn release_refuses_attached_transition_and_missing_provider_history() {
        let host = host();
        let (directory, source, _handle) = closed_history(&host);
        host.lock()
            .unwrap()
            .chats
            .get_mut(&source.id)
            .unwrap()
            .subscribers
            .insert(4, false);
        assert!(
            inspect(&host, source.id)
                .unwrap()
                .release_blocked
                .unwrap()
                .contains("attached")
        );
        host.lock()
            .unwrap()
            .chats
            .get_mut(&source.id)
            .unwrap()
            .subscribers
            .clear();
        host.lock()
            .unwrap()
            .chat_spawning
            .insert(source.id, source.clone());
        assert!(
            inspect(&host, source.id)
                .unwrap()
                .release_blocked
                .unwrap()
                .contains("transition")
        );
        host.lock().unwrap().chat_spawning.clear();
        std::fs::remove_dir_all(directory.path().join("claude")).unwrap();
        assert!(
            inspect(&host, source.id)
                .unwrap()
                .release_blocked
                .unwrap()
                .contains("history")
        );
        assert!(host.lock().unwrap().chats.contains_key(&source.id));
    }
    #[test]
    fn exact_recovery_has_one_reservation_and_rejects_the_second_before_setup() {
        let host = host();
        let (_directory, source, _) = closed_history(&host);
        host.lock().unwrap().native_chat_capacity = 1;
        let first = Reservation::new(&host, &source).unwrap();
        assert!(first.is_some());
        assert!(Reservation::new(&host, &source).is_err());
        assert_eq!(usage(&host.lock().unwrap()), 1);
        drop(first);
        assert!(Reservation::new(&host, &source).unwrap().is_some());
    }

    #[test]
    fn live_read_only_paging_worker_can_be_released_without_provider_execution() {
        let host = host();
        let (_directory, source, handle) = history_reader(&host, false);
        assert!(!handle.is_completed());
        assert!(!handle.owns_work());
        let inspection = inspect(&host, source.id).unwrap();
        assert_eq!(inspection.release_blocked, None);
        release(&host, &inspection.proof, false).unwrap();
        assert!(handle.is_completed());
        assert!(!host.lock().unwrap().chats.contains_key(&source.id));
        let (_, mut adapter) = chat::protocol::create(&source.launch).unwrap();
        assert!(!adapter.initialize().unwrap().events.is_empty());
    }

    #[test]
    fn live_writers_are_protected_and_completed_writers_require_full_coverage() {
        struct Idle;
        impl chat::protocol::Adapter for Idle {
            fn initialize(&mut self) -> Result<chat::protocol::Step, String> {
                Ok(Default::default())
            }
            fn command(&mut self, _: chat::Command) -> Result<chat::protocol::Step, String> {
                Ok(Default::default())
            }
            fn receive(&mut self, _: serde_json::Value) -> Result<chat::protocol::Step, String> {
                Ok(Default::default())
            }
        }
        let host = host();
        let (_directory, source, _) = closed_history(&host);
        let mut recipe = source.launch.recipe.clone();
        recipe.program = "/bin/cat".into();
        recipe.working_directory = std::env::current_dir().unwrap();
        let spec = chat::protocol::ProcessSpec::from(&recipe);
        let (_, mut history) = chat::protocol::create_history(&source.launch).unwrap();
        let item = history
            .initialize()
            .unwrap()
            .events
            .into_iter()
            .find_map(|event| {
                if let chat::Event::History { items, .. } = event {
                    items.into_iter().next()
                } else {
                    None
                }
            })
            .expect("nonempty saved history");
        // Observing an otherwise identical item live stamps local times. These
        // are resident data too and must not be silently discarded on release.
        let mut initial = chat::protocol::Step {
            events: vec![chat::Event::Item(item)],
            ..Default::default()
        };
        let store = chat::runtime::preflight(&spec, &mut initial).unwrap();
        let handle = Handle::start(
            spec,
            Box::new(Idle),
            initial,
            store,
            source.launch.session_id.clone(),
            |_| {},
        )
        .unwrap();
        host.lock()
            .unwrap()
            .chats
            .get_mut(&source.id)
            .unwrap()
            .handle = handle.clone();
        assert!(Reservation::new(&host, &source).is_err());
        assert!(
            inspect(&host, source.id)
                .unwrap()
                .release_blocked
                .unwrap()
                .contains("owns")
        );
        handle.close().unwrap();
        assert!(!handle.owns_work());
        assert!(
            inspect(&host, source.id)
                .unwrap()
                .release_blocked
                .unwrap()
                .contains("cover")
        );
        assert!(host.lock().unwrap().chats.contains_key(&source.id));
    }

    fn completed_writer(host: &SharedHost) -> (tempfile::TempDir, Session, Handle) {
        use std::os::unix::fs::PermissionsExt;
        let (directory, mut source, _) = closed_history(host);
        std::fs::create_dir(&source.launch.recipe.working_directory).unwrap();
        let program = directory.path().join("completed-provider");
        // A real pipe transport consumes initialization and exits normally.
        // Its transcript comes from the real Claude history adapter, not a
        // hand-built Store or a history reader relabeled as a writer.
        std::fs::write(&program, "#!/bin/sh\nread -r initialization\nexit 0\n").unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        source.launch.recipe.program = program.to_string_lossy().into_owned();
        // Enter through the real daemon start path: ordinary UI writers also
        // receive a self-delegation capability, even without a managed Agent.
        let old = host.lock().unwrap().chats.remove(&source.id).unwrap();
        drop(old);
        let ServerResponse::ChatAttached { session, .. } = process_inner(
            host,
            73,
            ClientRequest::ChatStart {
                session: source,
                source: None,
                terminate_source: false,
                execution_environment: Default::default(),
            },
        )
        .unwrap() else {
            panic!("writer start response")
        };
        let source = session;
        let handle = host.lock().unwrap().chats[&source.id].handle.clone();
        let registry = super::super::super::agent_execution::registry(host).unwrap();
        assert!(
            registry
                .lock()
                .unwrap()
                .retained_agent_authority_source(source.id)
                .unwrap()
                .is_some(),
            "real startup must register self authority; a manually constructed actor misses this contract"
        );
        assert!(
            registry
                .lock()
                .unwrap()
                .agent_for_session(source.id)
                .unwrap()
                .is_none()
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        while !handle.is_completed() {
            assert!(Instant::now() < deadline, "writer did not finish teardown");
            thread::sleep(Duration::from_millis(5));
        }
        handle.close().unwrap();
        assert!(!handle.is_read_only_history());
        assert!(
            !handle.owns_work(),
            "writer cleanup: {:?}",
            handle.failure()
        );
        let snapshot = handle.snapshot(None).unwrap();
        assert!(snapshot.exited);
        assert!(snapshot.read_only_reason.is_none());
        assert!(snapshot.items.iter().any(|item| matches!(&item.content, chat::ItemContent::Text { text, .. } if text == "durable history body")));
        assert!(
            inspect(host, source.id)
                .unwrap()
                .release_blocked
                .unwrap()
                .contains("attached")
        );
        process_inner(host, 73, ClientRequest::ChatDetach { chat_id: source.id }).unwrap();
        (directory, source, handle)
    }

    #[test]
    fn completed_writer_release_checks_saved_bytes_and_stale_generation() {
        for changed in [false, true] {
            let host = host();
            let (_directory, source, handle) = completed_writer(&host);
            let registry = super::super::super::agent_execution::registry(&host).unwrap();
            let token = registry
                .lock()
                .unwrap()
                .retained_agent_authority(source.id)
                .unwrap();
            let inspection = inspect(&host, source.id).unwrap();
            assert!(registry.lock().unwrap().execution_origin(&token).is_ok());
            assert_eq!(inspection.release_blocked, None);
            let mut stale = inspection.proof.clone();
            stale.generation += 1;
            assert!(release(&host, &stale, false).is_err());
            assert!(host.lock().unwrap().chats.contains_key(&source.id));
            if changed {
                std::fs::write(provider_history_path(&source), "").unwrap();
                assert!(release(&host, &inspection.proof, false).is_err());
                assert!(host.lock().unwrap().chats.contains_key(&source.id));
                assert!(handle.snapshot(None).unwrap().items.iter().any(|item| matches!(&item.content, chat::ItemContent::Text { text, .. } if text == "durable history body")));
            } else {
                release(&host, &inspection.proof, false).unwrap();
                assert!(!host.lock().unwrap().chats.contains_key(&source.id));
                assert!(registry.lock().unwrap().execution_origin(&token).is_err());
            }
            if changed {
                assert!(registry.lock().unwrap().execution_origin(&token).is_ok());
            }
        }
    }

    fn provider_history_path(source: &Session) -> std::path::PathBuf {
        let project: String = source
            .launch
            .recipe
            .working_directory
            .to_str()
            .unwrap()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        std::path::PathBuf::from(&source.launch.recipe.environment["CLAUDE_CONFIG_DIR"])
            .join("projects")
            .join(project)
            .join(format!(
                "{}.jsonl",
                source.launch.session_id.as_ref().unwrap()
            ))
    }

    #[test]
    fn truncated_or_changed_provider_history_cannot_release_the_only_cached_body() {
        for changed in ["", "changed provider body"] {
            let host = host();
            let (_directory, source, handle) = closed_history(&host);
            let proof = inspect(&host, source.id).unwrap().proof;
            let path = provider_history_path(&source);
            let text = std::fs::read_to_string(&path).unwrap();
            std::fs::write(
                &path,
                if changed.is_empty() {
                    String::new()
                } else {
                    text.replace("durable history body", changed)
                },
            )
            .unwrap();
            assert!(
                inspect(&host, source.id)
                    .unwrap()
                    .release_blocked
                    .unwrap()
                    .contains("does not cover")
            );
            assert!(release(&host, &proof, false).is_err());
            assert!(host.lock().unwrap().chats.contains_key(&source.id));
            assert!(handle.snapshot(None).unwrap().items.iter().any(|item| matches!(&item.content, chat::ItemContent::Text { text, .. } if text.contains("durable history body"))));
        }
    }

    #[test]
    fn pending_launches_and_retained_managed_authorities_block_resident_release() {
        use crate::agent_execution::{
            Destination, HostedSession, Origin, Spec, Submit, Surface, Target,
        };
        for pending in [false, true] {
            let host = host();
            let (directory, source, _) = closed_history(&host);
            let mut registry = super::super::super::model::Registry::at(
                &directory.path().join("workspace.sqlite"),
            )
            .unwrap();
            let owned = HostedSession::Chat(source.clone());
            if pending {
                registry
                    .apply(&crate::workspace_model::ModelEdit::OpenProject {
                        repository: directory.path().into(),
                        name: "Fixture".into(),
                        kind: crate::model::ProjectKind::Folder,
                    })
                    .unwrap();
                let accepted = registry
                    .submit_agent(
                        &Origin::User,
                        &Submit {
                            name: None,
                            destination: Destination::New(Spec {
                                provider: Provider::Claude,
                                account: None,
                                target: Target {
                                    repository: directory.path().into(),
                                    directory: directory.path().into(),
                                    placement: None,
                                },
                                surface: Surface::Chat,
                                model: None,
                                permission_mode: None,
                                reasoning_effort: None,
                            }),
                            prompt: "private pending fixture".into(),
                            idempotency_key: "pending-release-test".into(),
                        },
                        1,
                    )
                    .unwrap();
                registry
                    .reserve_agent_launch(accepted.agent.id, &owned)
                    .unwrap();
            } else {
                registry.register_agent_authority(&owned).unwrap();
            }
            assert!(registry.agent_for_session(source.id).unwrap().is_none());
            host.lock().unwrap().execution_registry =
                Some(Arc::new(std::sync::Mutex::new(registry)));
            assert!(
                inspect(&host, source.id)
                    .unwrap()
                    .release_blocked
                    .unwrap()
                    .contains("managed agent")
            );
            assert!(host.lock().unwrap().chats.contains_key(&source.id));
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn account_operation_refuses_inspection_before_opening_provider_history() {
        use std::{
            io::Read,
            os::fd::{AsRawFd, FromRawFd},
        };
        let host = host();
        let (directory, source, _) = closed_history(&host);
        let path = provider_history_path(&source);
        let other = directory.path().join("reserved-account");
        std::fs::create_dir(&other).unwrap();
        let homes = crate::account::home::Homes::at(directory.path().join("homes"));
        let store = crate::account::registry::Store::at(directory.path().join("accounts"));
        let reservation = super::super::super::account_lifecycle::Reservation::test_install(
            &host,
            &[crate::account::Account::linked(
                Provider::Claude,
                uuid::Uuid::new_v4(),
                other,
            )],
            store,
            &homes,
        )
        .unwrap();
        let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        assert!(fd >= 0);
        let mut watcher = unsafe { std::fs::File::from_raw_fd(fd) };
        let cpath = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        assert!(
            unsafe {
                libc::inotify_add_watch(
                    watcher.as_raw_fd(),
                    cpath.as_ptr(),
                    libc::IN_OPEN | libc::IN_ACCESS,
                )
            } >= 0
        );
        assert!(inspect(&host, source.id).is_err());
        let request = ClientRequest::ChatReleaseInspect { chat_id: source.id };
        assert!(super::super::super::account_lifecycle::admit(&host, &request).is_err());
        let mut events = [0_u8; 256];
        assert_eq!(
            watcher.read(&mut events).unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        drop(reservation);
        assert_eq!(inspect(&host, source.id).unwrap().release_blocked, None);
    }

    #[test]
    fn history_release_reservation_prevents_a_new_attached_view() {
        let host = host();
        let (_directory, source, _) = closed_history(&host);
        host.lock()
            .unwrap()
            .chat_spawning
            .insert(source.id, source.clone());
        assert!(
            process_inner(
                &host,
                9,
                ClientRequest::ChatAttach {
                    session: source.clone()
                }
            )
            .is_err()
        );
        assert!(
            host.lock().unwrap().chats[&source.id]
                .subscribers
                .is_empty()
        );
        host.lock().unwrap().chat_spawning.clear();
        assert!(matches!(
            process_inner(
                &host,
                9,
                ClientRequest::ChatAttach {
                    session: source.clone()
                }
            )
            .unwrap(),
            ServerResponse::ChatAttached { .. }
        ));
        assert!(
            host.lock().unwrap().chats[&source.id]
                .subscribers
                .contains_key(&9)
        );
    }
    #[test]
    fn unavailable_managed_registry_preserves_reader_until_verified_empty_registry_returns() {
        let host = host();
        let (_directory, source, handle) = history_reader(&host, false);
        let proof = inspect(&host, source.id).unwrap().proof;
        let verified_registry = host.lock().unwrap().execution_registry.take().unwrap();
        let blocked = inspect(&host, source.id).unwrap();
        assert!(
            blocked
                .release_blocked
                .unwrap()
                .contains("registry is unavailable")
        );
        assert!(release(&host, &proof, false).is_err());
        assert!(
            !handle.is_completed(),
            "unknown ownership must not even stop paging"
        );
        assert!(
            host.lock().unwrap().chats[&source.id]
                .handle
                .same_actor(&handle)
        );
        assert!(
            verified_registry
                .lock()
                .unwrap()
                .retained_agent_sources(source.id)
                .unwrap()
                .is_empty()
        );
        host.lock().unwrap().execution_registry = Some(verified_registry);
        let fresh = inspect(&host, source.id).unwrap();
        assert_eq!(fresh.release_blocked, None);
        release(&host, &fresh.proof, false).unwrap();
        assert!(handle.is_completed());
        assert!(!host.lock().unwrap().chats.contains_key(&source.id));
    }

    fn restore_client(
        host: &SharedHost,
        source: Session,
        client_id: ClientId,
    ) -> (
        Result<chat::client::ClientHandle, chat::StartFailure>,
        std::thread::JoinHandle<()>,
    ) {
        let (client, server) = std::os::unix::net::UnixStream::pair().unwrap();
        let server_host = host.clone();
        let server = thread::spawn(move || {
            super::super::super::client::serve(server, client_id, server_host);
        });
        (
            chat::client::ClientHandle::fixture_restore(client, source),
            server,
        )
    }

    #[test]
    fn pending_inventory_proves_the_daemon_without_replacing_its_reservation() {
        let host = host();
        let mut source = session();
        source.launch.session_id = Some("12345678-1234-4234-8234-123456789abc".into());
        assert_eq!(source.proof, None);
        let reservation = Reservation::new(&host, &source).unwrap().unwrap();
        let ServerResponse::ChatListed { sessions } =
            process_inner(&host, 61, ClientRequest::ChatList).unwrap()
        else {
            panic!("pending inventory response")
        };
        let mut expected = source.clone();
        expected.proof = Some(host.lock().unwrap().proof().clone());
        assert_eq!(sessions, vec![expected]);
        assert_eq!(
            host.lock().unwrap().chat_spawning.get(&source.id),
            Some(&source)
        );
        drop(reservation);
        assert!(host.lock().unwrap().chat_spawning.is_empty());
        assert!(host.lock().unwrap().chats.is_empty());
    }

    #[test]
    fn proofless_client_restore_of_a_pending_start_reports_transition_not_identity_mismatch() {
        let host = host();
        let mut source = session();
        source.launch.session_id = Some("12345678-1234-4234-8234-123456789abc".into());
        assert_eq!(source.proof, None);
        let reservation = Reservation::new(&host, &source).unwrap().unwrap();
        // This is the real client handshake, inventory lookup and attach over
        // the actual daemon request dispatcher, not a direct ChatStart call.
        let (result, server) = restore_client(&host, source.clone(), 62);
        let failure = match result {
            Err(failure) => failure,
            Ok(_) => panic!("a pending provider start must not be replaced"),
        };
        server.join().unwrap();
        assert_eq!(failure.recovery, chat::Recovery::Retry);
        assert_eq!(failure.message, "Chat is already starting or closing");
        assert_eq!(
            host.lock().unwrap().chat_spawning.get(&source.id),
            Some(&source)
        );
        assert!(host.lock().unwrap().chats.is_empty());
        drop(reservation);
        assert!(host.lock().unwrap().chat_spawning.is_empty());
    }

    fn wait_for_client_history(client: &chat::client::ClientHandle, read_only: bool) {
        client.set_visible(true).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let snapshot = client.snapshot();
            if (!read_only || snapshot.read_only_reason.is_some())
                && snapshot.items.iter().any(|item| matches!(&item.content, chat::ItemContent::Text { text, .. } if text.contains("durable history body")))
            {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "real client did not receive saved history"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn released_history_reopens_through_real_client_restore_even_when_directory_returns() {
        use std::os::unix::fs::PermissionsExt;
        for (writer, directory_returns) in
            [(false, false), (false, true), (true, false), (true, true)]
        {
            let host = host();
            let (directory, mut source, _) = if writer {
                let result = completed_writer(&host);
                std::fs::remove_dir(&result.1.launch.recipe.working_directory).unwrap();
                result
            } else {
                history_reader(&host, false)
            };
            let marker = directory.path().join("provider-started");
            let program = if writer {
                std::path::PathBuf::from(&source.launch.recipe.program)
            } else {
                directory.path().join("provider")
            };
            std::fs::write(
                &program,
                format!(
                    "#!/bin/sh\nprintf started > '{}'\nexit 91\n",
                    marker.display()
                ),
            )
            .unwrap();
            std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
            source.launch.recipe.program = program.to_string_lossy().into_owned();
            host.lock()
                .unwrap()
                .chats
                .get_mut(&source.id)
                .unwrap()
                .session = source.clone();
            let (first, server) = restore_client(&host, source.clone(), 41);
            let first = first.unwrap();
            wait_for_client_history(&first, !writer);
            assert_eq!(first.session(), source);
            assert!(first.snapshot().items.iter().any(|item| matches!(&item.content, chat::ItemContent::Text { text, .. } if text.contains("durable history body"))));
            drop(first);
            server.join().unwrap();
            assert!(
                host.lock().unwrap().chats[&source.id]
                    .subscribers
                    .is_empty()
            );

            let (client, server) = std::os::unix::net::UnixStream::pair().unwrap();
            let server_host = host.clone();
            let server =
                thread::spawn(move || super::super::super::client::serve(server, 42, server_host));
            let mut control = crate::control::ControlClient::from_stream(client).unwrap();
            control.authenticate(Default::default()).unwrap();
            let ServerResponse::ChatReleaseInspected { inspection } = control
                .fixture_request(ClientRequest::ChatReleaseInspect { chat_id: source.id })
                .unwrap()
            else {
                panic!("release inspection response")
            };
            assert_eq!(inspection.release_blocked, None);
            assert!(matches!(
                control
                    .fixture_request(ClientRequest::ChatRelease {
                        proof: inspection.proof,
                        dry_run: false
                    })
                    .unwrap(),
                ServerResponse::ChatReleased { dry_run: false, .. }
            ));
            drop(control);
            server.join().unwrap();
            assert!(!host.lock().unwrap().chats.contains_key(&source.id));
            if directory_returns {
                std::fs::create_dir(&source.launch.recipe.working_directory).unwrap();
            }

            let pending = session();
            {
                let mut state = host.lock().unwrap();
                state.native_chat_capacity = 1;
                state.chat_spawning.insert(pending.id, pending.clone());
            }
            let (full, server) = restore_client(&host, source.clone(), 43);
            let failure = match full {
                Err(failure) => failure,
                Ok(_) => panic!("full capacity restored a new resident"),
            };
            assert!(failure.message.contains("capacity reached"));
            server.join().unwrap();
            assert!(!host.lock().unwrap().chats.contains_key(&source.id));
            host.lock().unwrap().chat_spawning.remove(&pending.id);

            let (restored, server) = restore_client(&host, source.clone(), 44);
            let restored = restored.unwrap();
            wait_for_client_history(&restored, true);
            assert_eq!(restored.session(), source);
            assert!(restored.snapshot().read_only_reason.is_some());
            assert!(restored.snapshot().items.iter().any(|item| matches!(&item.content, chat::ItemContent::Text { text, .. } if text.contains("durable history body"))));
            assert!(
                !marker.exists(),
                "restoring history must never execute the provider program"
            );
            assert!(
                host.lock().unwrap().chats[&source.id]
                    .handle
                    .is_read_only_history()
            );
            assert!(!host.lock().unwrap().chats[&source.id].handle.owns_work());
            // Explicit continuation uses the exact currently authenticated
            // reader descriptor, not a fabricated retirement or new source.
            let ServerResponse::ChatResumePrepared { session: prepared } = process_inner(
                &host,
                0,
                ClientRequest::ChatPrepareResume {
                    source: source.clone(),
                },
            )
            .unwrap() else {
                panic!("exact history preparation expected")
            };
            assert_eq!(prepared, source);
            let mut changed = source.clone();
            changed.launch.recipe.working_directory = directory.path().join("another-project");
            assert!(
                process_inner(
                    &host,
                    0,
                    ClientRequest::ChatPrepareResume { source: changed }
                )
                .is_err()
            );
            host.lock()
                .unwrap()
                .chat_spawning
                .insert(source.id, source.clone());
            assert!(
                process_inner(
                    &host,
                    0,
                    ClientRequest::ChatPrepareResume {
                        source: source.clone()
                    }
                )
                .is_err()
            );
            host.lock().unwrap().chat_spawning.remove(&source.id);
            assert!(
                !marker.exists(),
                "preparation must never execute the provider"
            );
            drop(restored);
            server.join().unwrap();
            // Join the owned reader before dropping its host; a final callback
            // must not become the last host owner and drop its own worker.
            let reader = host
                .lock()
                .unwrap()
                .chats
                .remove(&source.id)
                .unwrap()
                .handle;
            reader.close().unwrap();
        }
    }

    #[test]
    fn missing_saved_history_refuses_unknown_ownership_and_native_writer_descriptors() {
        let host = host();
        let (_directory, source, _) = closed_history(&host);
        let proof = inspect(&host, source.id).unwrap().proof;
        release(&host, &proof, false).unwrap();
        let registry = host.lock().unwrap().execution_registry.take().unwrap();
        assert!(
            process_inner(
                &host,
                51,
                ClientRequest::ChatAttach {
                    session: source.clone()
                }
            )
            .is_err()
        );
        assert!(host.lock().unwrap().chats.is_empty());
        assert!(host.lock().unwrap().chat_spawning.is_empty());
        host.lock().unwrap().execution_registry = Some(registry.clone());
        registry
            .lock()
            .unwrap()
            .register_agent_authority(&crate::agent_execution::HostedSession::Chat(source.clone()))
            .unwrap();
        assert!(
            process_inner(
                &host,
                51,
                ClientRequest::ChatAttach {
                    session: source.clone()
                }
            )
            .is_err()
        );
        registry
            .lock()
            .unwrap()
            .retire_agent_authority(&crate::agent_execution::HostedSession::Chat(source.clone()))
            .unwrap();
        let mut durable = source.clone();
        durable.native_continuity = chat::NativeContinuity::Durable {
            provider_version: "test".into(),
        };
        assert!(process_inner(&host, 51, ClientRequest::ChatAttach { session: durable }).is_err());
        let mut wrong_daemon = source;
        wrong_daemon.proof = None;
        assert!(
            process_inner(
                &host,
                51,
                ClientRequest::ChatAttach {
                    session: wrong_daemon
                }
            )
            .is_err()
        );
        assert!(host.lock().unwrap().chats.is_empty());
        assert!(host.lock().unwrap().chat_spawning.is_empty());
    }

    #[test]
    fn old_epoch_history_recovery_uses_real_handshake_without_launching_65_providers() {
        use std::os::unix::fs::PermissionsExt;
        let started = Instant::now();
        let host = host();
        let (directory, template, _) = closed_history(&host);
        let original = inspect(&host, template.id).unwrap();
        release(&host, &original.proof, false).unwrap();
        let program = directory.path().join("must-not-run-provider");
        let marker = directory.path().join("provider-was-started");
        std::fs::write(
            &program,
            format!("#!/bin/sh\ntouch '{}'\nexit 91\n", marker.display()),
        )
        .unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        for index in 0..65 {
            // Half the run exercises a restored cwd. Neither filesystem state
            // may select the provider constructor instead of forced History.
            if index == 32 {
                std::fs::create_dir(&template.launch.recipe.working_directory).unwrap();
            }
            let mut source = template.clone();
            source.id = TerminalId::new();
            source.launch.recipe.program = program.to_string_lossy().into_owned();
            source.launch.session_id = Some(uuid::Uuid::new_v4().to_string());
            source.proof.as_mut().unwrap().host_incarnation =
                HostIncarnation::new("previous-daemon");
            let transcript = provider_history_path(&source);
            std::fs::write(&transcript, serde_json::json!({"type":"user","uuid":format!("item-{index}"),"parentUuid":null,"sessionId":source.launch.session_id,"message":{"role":"user","content":"durable history body"}}).to_string()+"\n").unwrap();
            let (client, server) = restore_client(&host, source.clone(), 100 + index);
            let client = client.unwrap();
            wait_for_client_history(&client, true);
            let mut expected = source.clone();
            expected.proof = Some(host.lock().unwrap().proof().clone());
            assert_eq!(client.session(), expected);
            let witness = client
                .authenticated_upgrade_recovery()
                .expect("actual Resolve/Attach witness");
            assert!(witness.validate());
            assert_eq!(witness.source(), &source);
            assert_eq!(witness.replacement(), &expected);
            {
                let state = host.lock().unwrap();
                let reader = &state.chats[&source.id];
                assert!(reader.handle.is_read_only_history());
                assert!(!reader.handle.owns_work());
                assert!(!reader.subscribers.contains_key(&0));
                assert!(!reader.observations.contains_key(&0));
            }
            assert!(!marker.exists(), "automatic restore launched a provider");
            if index == 0 {
                let mut older_alias = source.clone();
                older_alias.proof.as_mut().unwrap().host_incarnation =
                    HostIncarnation::new("still-older-window");
                let (second, second_server) = restore_client(&host, older_alias.clone(), 301);
                let second = second.unwrap();
                wait_for_client_history(&second, true);
                assert_eq!(second.session(), expected);
                let second_witness = second.authenticated_upgrade_recovery().unwrap();
                assert!(second_witness.validate());
                assert_eq!(second_witness.source(), &older_alias);
                let mut changed_account = older_alias.clone();
                changed_account
                    .launch
                    .recipe
                    .environment
                    .insert("CLAUDE_CONFIG_DIR".into(), "/different-account".into());
                assert!(register_saved_history_alias(&host, &changed_account).is_err());
                drop(second);
                second_server.join().unwrap();
            }
            drop(client);
            server.join().unwrap();
            let release_proof = inspect(&host, source.id).unwrap();
            assert_eq!(release_proof.release_blocked, None);
            release(&host, &release_proof.proof, false).unwrap();
        }
        assert!(host.lock().unwrap().chats.is_empty());
        assert!(!marker.exists());
        eprintln!(
            "65 exact history handshakes and releases: {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn history_preview_preserves_an_attached_view_and_unrelated_live_consent() {
        struct Idle;
        impl chat::protocol::Adapter for Idle {
            fn initialize(&mut self) -> Result<chat::protocol::Step, String> {
                Ok(Default::default())
            }
            fn command(&mut self, _: chat::Command) -> Result<chat::protocol::Step, String> {
                Ok(Default::default())
            }
            fn receive(&mut self, _: serde_json::Value) -> Result<chat::protocol::Step, String> {
                Ok(Default::default())
            }
        }
        let host = host();
        let (directory, source, _) = history_reader(&host, false);
        let (view, server) = restore_client(&host, source.clone(), 71);
        let view = view.unwrap();
        wait_for_client_history(&view, true);
        // A genuine fixture-owned pipe process supplies unrelated live work.
        let mut live = session();
        live.proof = Some(host.lock().unwrap().proof().clone());
        live.launch.recipe.program = "/bin/cat".into();
        live.launch.recipe.working_directory = directory.path().to_path_buf();
        let spec = chat::protocol::ProcessSpec::from(&live.launch.recipe);
        let mut initial = chat::protocol::Step::default();
        let store = chat::runtime::preflight(&spec, &mut initial).unwrap();
        let handle = Handle::start(spec, Box::new(Idle), initial, store, None, |_| {}).unwrap();
        host.lock().unwrap().chats.insert(
            live.id,
            Record {
                execution_instance: uuid::Uuid::new_v4(),
                console_recipe: None,
                paired_terminal: None,
                session: live.clone(),
                handle: handle.clone(),
                source_process: None,
                subscribers: BTreeMap::new(),
                observations: BTreeMap::new(),
                completion_observed: false,
                returning_terminal: false,
            },
        );
        let preview = |client_id| {
            let (stream, socket) = std::os::unix::net::UnixStream::pair().unwrap();
            let server_host = host.clone();
            let worker = thread::spawn(move || {
                super::super::super::client::serve(socket, client_id, server_host)
            });
            let proof = host.lock().unwrap().proof().clone();
            crate::host::upgrade_inspect::InspectionClient::fixture_inspect_history(
                stream, &source, proof,
            )
            .unwrap();
            worker.join().unwrap();
        };
        preview(72);
        let generation = host.lock().unwrap().inventory_generation;
        let before = host
            .lock()
            .unwrap()
            .upgrade_impact(crate::update::impact::Policy::WhenIdle)
            .unwrap();
        assert!(before.sessions.iter().any(|session| session.id == live.id));
        preview(73);
        let after = host
            .lock()
            .unwrap()
            .upgrade_impact(crate::update::impact::Policy::WhenIdle)
            .unwrap();
        assert_eq!(host.lock().unwrap().inventory_generation, generation);
        assert!(before.preserves_consent(&after));
        assert!(!handle.is_completed());
        assert!(view.is_connected());
        assert_eq!(view.session(), source);
        assert!(
            host.lock().unwrap().chats[&source.id]
                .subscribers
                .contains_key(&71)
        );
        assert!(
            inspect(&host, source.id)
                .unwrap()
                .release_blocked
                .unwrap()
                .contains("attached")
        );
        drop(view);
        server.join().unwrap();
        handle.close().unwrap();
        host.lock().unwrap().chats.remove(&live.id);
        let inspection = inspect(&host, source.id).unwrap();
        release(&host, &inspection.proof, false).unwrap();
    }
}

pub(in crate::host) fn status(host: &SharedHost) -> crate::terminal_ipc::ChatCapacity {
    let state = host.lock().unwrap_or_else(|p| p.into_inner());
    status_from_state(&state)
}

pub(super) fn status_from_state(state: &HostState) -> crate::terminal_ipc::ChatCapacity {
    crate::terminal_ipc::ChatCapacity {
        resident: state.chats.len() as u32,
        spawning: state.chat_spawning.len() as u32,
        limit: state.native_chat_capacity as u32,
        used: usage(&state) as u32,
        residents: state
            .chats
            .iter()
            .take(4096)
            .map(|(&id, record)| crate::terminal_ipc::ChatCapacityResident {
                chat_id: id,
                provider: record.session.launch.provider,
                status: if state.chat_spawning.contains_key(&id) {
                    "starting".into()
                } else if record.handle.is_read_only_history() {
                    "read-only history".into()
                } else if record.handle.is_completed() {
                    "closed".into()
                } else {
                    format!("{:?}", record.handle.activity())
                },
                release_blocked: if record.handle.owns_work() {
                    Some("Actor owns live or unconfirmed provider work".into())
                } else if !record.subscribers.is_empty() {
                    Some("A client is attached".into())
                } else if !record.handle.is_read_only_history() && !record.handle.is_completed() {
                    Some("Writer teardown is unconfirmed".into())
                } else if state.chat_spawning.contains_key(&id)
                    || state.native_closing.contains_key(&id)
                    || state.chat_handoffs.contains_key(&id)
                    || record.returning_terminal
                {
                    Some("Ownership transition is pending".into())
                } else if record.paired_terminal.is_some() || record.session.terminal.is_some() {
                    Some("Paired terminal ownership remains".into())
                } else {
                    None
                },
            })
            .collect(),
        omitted_residents: state.chats.len().saturating_sub(4096) as u32,
    }
}

fn inspected_record(
    state: &HostState,
    id: TerminalId,
) -> Result<(crate::terminal_ipc::ChatReleaseInspection, Handle, Session), ProtocolError> {
    let record = state
        .chats
        .get(&id)
        .ok_or_else(|| unavailable("Native chat is no longer resident"))?;
    // Retained actors are never eligible for resident eviction. Inspect only
    // cached identity metadata while holding HostState, without a helper RPC.
    if record.handle.actor_identity().is_some() {
        let mut session = record.session.clone();
        session.launch.session_id = record.handle.session_id();
        session.native_continuity = record.handle.native_continuity();
        return Ok((crate::terminal_ipc::ChatReleaseInspection {
            proof: crate::terminal_ipc::ChatReleaseProof {
                session,
                generation: record.handle.resident_generation(),
                execution_instance: record.execution_instance,
            },
            release_blocked: Some("The actor retains native provider ownership; use its authenticated close operation".into()),
        }, record.handle.clone(), record.session.clone()));
    }
    let ServerResponse::ChatAttached { session, snapshot } =
        session_snapshot(&record.session, &record.handle).map_err(unavailable)?
    else {
        unreachable!()
    };
    let blocked = if state.shutting_down {
        Some("Daemon is shutting down")
    } else if state.chat_spawning.contains_key(&id)
        || state.native_closing.contains_key(&id)
        || state.chat_handoffs.contains_key(&id)
        || record.returning_terminal
    {
        Some("A native ownership transition is pending")
    } else if record.handle.owns_work() {
        Some(
            "The actor is live or still owns unconfirmed backend work; idle does not authorize release",
        )
    } else if !record.subscribers.is_empty() {
        Some("A client is attached; detach its view before releasing the resident actor")
    } else if record.paired_terminal.is_some() || session.terminal.is_some() {
        Some("The actor retains a paired terminal")
    } else if !matches!(
        session.native_continuity,
        chat::NativeContinuity::ReconnectOnly
    ) || session.owned_codex_listener.is_some()
    {
        Some("The actor retains native provider ownership; use its authenticated close operation")
    } else if snapshot.busy
        || snapshot.waiting
        || !snapshot.requests.is_empty()
        || !record.handle.background_subagents().is_empty()
    {
        Some("The actor still reports pending or delegated work")
    } else if !snapshot.subagents.is_empty() {
        Some("Child transcript preservation is not yet verified; its resident history is preserved")
    } else if !record.handle.is_read_only_history() && !record.handle.is_completed() {
        Some("The writer has not finished provider teardown")
    } else if !snapshot.exited
        || (record.handle.is_read_only_history() && snapshot.read_only_reason.is_none())
    {
        Some("The history reader or completed writer has not settled")
    } else if session.launch.session_id.is_none() {
        Some("The provider conversation identity is unconfirmed")
    } else {
        None
    };
    Ok((
        crate::terminal_ipc::ChatReleaseInspection {
            proof: crate::terminal_ipc::ChatReleaseProof {
                session,
                generation: snapshot.generation,
                execution_instance: record.execution_instance,
            },
            release_blocked: blocked.map(str::to_owned),
        },
        record.handle.clone(),
        record.session.clone(),
    ))
}

fn release_kind(handle: &Handle) -> super::super::model::ResidentRelease {
    if handle.is_read_only_history() {
        super::super::model::ResidentRelease::ReadOnlyHistory
    } else {
        super::super::model::ResidentRelease::CompletedWriter
    }
}

pub(in crate::host) fn inspect(
    host: &SharedHost,
    id: TerminalId,
) -> Result<crate::terminal_ipc::ChatReleaseInspection, ProtocolError> {
    let (mut inspection, handle, source) =
        inspected_record(&host.lock().unwrap_or_else(|p| p.into_inner()), id)?;
    if inspection.release_blocked.is_some() {
        return Ok(inspection);
    }
    super::super::account_lifecycle::ensure_idle(host)?;
    // A writer's own delegation capability is separate from managed work.
    let ownership = super::super::agent_execution::registry(host).and_then(|registry| {
        let result = registry
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .inspect_resident_release(
                &crate::agent_execution::HostedSession::Chat(source),
                release_kind(&handle),
            );
        result
    });
    if let Err(error) = ownership {
        inspection.release_blocked = Some(format!("Cannot verify managed ownership: {error}"));
        return Ok(inspection);
    }
    if inspection.release_blocked.is_none() {
        let reload = reload_history(&inspection.proof.session)
            .and_then(|reloaded| if handle.history_is_covered_by(&reloaded)? {
                Ok(())
            } else {
                Err("The reloaded provider history does not cover all resident transcript data; older cached pages or changed history remain protected".into())
            });
        if let Err(error) = reload {
            inspection.release_blocked =
                Some(format!("Saved provider history is not verified: {error}"));
        }
    }
    Ok(inspection)
}

fn reload_history(session: &Session) -> Result<chat::runtime::Store, String> {
    let (spec, _, mut initial) = load_history(session)?;
    chat::runtime::preflight(&spec, &mut initial)
}

type LoadedHistory = (
    chat::protocol::ProcessSpec,
    Box<dyn chat::protocol::Adapter>,
    chat::protocol::Step,
);

fn load_history(session: &Session) -> Result<LoadedHistory, String> {
    if has_native_owner(session).map_err(|error| error.message)? {
        return Err("An authenticated native provider owner remains".into());
    }
    let (spec, mut adapter) = chat::protocol::create_history(&session.launch)?;
    if !matches!(spec.transport, chat::protocol::TransportSpec::History) {
        return Err(
            "The saved conversation cannot currently be reopened as read-only history".into(),
        );
    }
    let initial = adapter.initialize()?;
    if !initial.writes.is_empty() {
        return Err("History reload attempted a provider write".into());
    }
    if initial
        .events
        .iter()
        .any(|event| matches!(event, chat::Event::HistoryError(_)))
    {
        return Err("Provider history could not be reloaded".into());
    }
    Ok((spec, adapter, initial))
}

fn require_unmanaged(host: &SharedHost, id: TerminalId) -> Result<(), String> {
    let registry = super::super::agent_execution::registry(host)?;
    if registry
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .retained_agent_sources(id)?
        .is_empty()
    {
        Ok(())
    } else {
        Err(
            "The actor belongs to a managed agent; use its supported owned-agent close operation"
                .into(),
        )
    }
}

/// A saved read-only descriptor can outlive its resident paging worker. This
/// path never resumes a provider: the constructor must prove History transport
/// again, even if the working directory changes while the request is admitted.
pub(super) fn restore_saved_history(
    host: &SharedHost,
    client: ClientId,
    session: Session,
) -> Result<ServerResponse, ProtocolError> {
    {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        validate_proof(session.proof.as_ref(), state.proof())?;
    }
    if session.terminal.is_some()
        || session.owned_codex_listener.is_some()
        || !matches!(
            session.native_continuity,
            chat::NativeContinuity::ReconnectOnly
        )
        || session.launch.provider != Provider::Claude
    {
        return Err(unavailable(
            "Exact native chat not found; only saved read-only history can be reattached without a resident actor",
        ));
    }
    let admission = Reservation::new_with_source(host, &session, None)?;
    {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        if state.chats.contains_key(&session.id) {
            return Err(unavailable(
                "The saved history became resident; retry its normal attach",
            ));
        }
    }
    super::super::account_lifecycle::ensure_idle(host)?;
    require_unmanaged(host, session.id).map_err(unavailable)?;
    let (mut spec, adapter, mut initial) = load_history(&session).map_err(unavailable)?;
    spec.messages = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .mailbox_seal
        .as_ref()
        .map(|seal| Arc::new(seal.recognizer()));
    let store = chat::runtime::preflight(&spec, &mut initial).map_err(unavailable)?;
    let environment = spec.environment.clone();
    let weak = Arc::downgrade(host);
    let id = session.id;
    let handle = Handle::start_owned(
        spec,
        adapter,
        initial,
        store,
        session.clone(),
        environment,
        move |update| changed(&weak, id, update),
    )
    .map_err(unavailable)?;
    let result = {
        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
        if state.shutting_down
            || state.chats.contains_key(&id)
            || state.chat_spawning.get(&id) != Some(&session)
        {
            Err(unavailable(
                "Native history ownership changed before installation",
            ))
        } else {
            let observation = Observation {
                after_generation: handle.observe_next_update(),
                ..Observation::default()
            };
            let record = Record {
                execution_instance: uuid::Uuid::new_v4(),
                console_recipe: None,
                paired_terminal: None,
                session,
                handle: handle.clone(),
                source_process: None,
                subscribers: if client == 0 {
                    BTreeMap::new()
                } else {
                    BTreeMap::from([(client, false)])
                },
                observations: if client == 0 {
                    BTreeMap::new()
                } else {
                    BTreeMap::from([(client, observation)])
                },
                completion_observed: false,
                returning_terminal: false,
            };
            let response =
                session_snapshot(&record.session, &record.handle).map_err(unavailable)?;
            record
                .handle
                .commit_native_installation()
                .map_err(unavailable)?;
            state.chats.insert(id, record);
            state.inventory_generation += 1;
            Ok(response)
        }
    };
    if result.is_err() {
        let _ = handle.close();
    }
    drop(admission);
    result
}

/// Reopen saved data across daemon epochs, without upgrading it to execution
/// ownership. The host issues the new proof only after the normal history
/// constructor verifies the exact thread/account and fences native owners and
/// managed agents. The old descriptor remains an exact Resolve alias.
pub(super) fn recover_saved_history(
    host: &SharedHost,
    source: &Session,
) -> Result<(), ProtocolError> {
    if !source.is_saved_claude_history() {
        return Err(unavailable(
            "This saved pane has no supported read-only history route",
        ));
    }
    let mut session = source.clone();
    session.proof = Some(
        host.lock()
            .unwrap_or_else(|p| p.into_inner())
            .proof()
            .clone(),
    );
    restore_saved_history(host, 0, session.clone())?;
    register_saved_history_alias(host, source)
}

/// Classify an authenticated paging reader for explicit resume. A retired
/// Agent allocation must be succeeded after independent retirement admission;
/// ordinary unmanaged readers keep their exact descriptor. Neither starts a
/// provider. Normal Start still repeats execution/account/admission checks.
pub(super) fn prepare_saved_history(
    host: &SharedHost,
    source: &Session,
) -> Result<Option<Session>, ProtocolError> {
    let state = host.lock().unwrap_or_else(|p| p.into_inner());
    let Some(record) = state.chats.get(&source.id) else {
        return Ok(None);
    };
    if !source.is_saved_claude_history() || !record.handle.is_read_only_history() {
        return Ok(None);
    }
    validate_proof(source.proof.as_ref(), state.proof())?;
    if record.session != *source
        || record.handle.session_id() != source.launch.session_id
        || record.handle.owns_work()
        || record.returning_terminal
        || state.shutting_down
        || state.chat_spawning.contains_key(&source.id)
        || state.native_closing.contains_key(&source.id)
        || state.native_actor_startups.contains_key(&source.id)
    {
        return Err(unavailable(
            "The exact saved-history reader changed; no provider was started",
        ));
    }
    let target = super::CloseTarget::capture(&state, record)?;
    drop(state);
    let registry = super::super::agent_execution::registry(host).map_err(unavailable)?;
    let retired = registry
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .has_retired_agent_allocation(source.id)
        .map_err(unavailable)?;
    if !retired {
        return Ok(Some(source.clone()));
    }
    require_unmanaged(host, source.id).map_err(unavailable)?;
    super::check_launch_account(&source.launch)?;
    super::retained_resume::prepare_history_successor(host, source).map_err(unavailable)?;
    // Explicit preparation closes only this exact paging reader through the
    // normal reservation/close path. This frees its slot without resurrecting
    // the canonical Agent or starting a provider, even at capacity one.
    super::close_captured(host, target)?;
    let mut successor = super::prepare_resume_successor(host, source.clone())?;
    // Presentation proof permits a crash before Start to reopen B as forced
    // read-only history. It grants no retained actor or execution authority.
    successor.proof = source.proof.clone();
    Ok(Some(successor))
}

/// An inactive window may retain a different former proof for this same
/// paging allocation. Admit that alias independently against the complete
/// current reader; an earlier alias cannot authenticate different content.
pub(super) fn register_saved_history_alias(
    host: &SharedHost,
    source: &Session,
) -> Result<(), ProtocolError> {
    let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
    let record = state
        .chats
        .get(&source.id)
        .ok_or_else(|| unavailable("Saved history changed during recovery"))?;
    if !source.is_saved_claude_history()
        || !record.handle.is_read_only_history()
        || record.handle.owns_work()
    {
        return Ok(()); // native execution keeps its separate lineage contract
    }
    let mut expected = source.clone();
    expected.proof = Some(state.proof().clone());
    if record.session != expected {
        return Err(unavailable(
            "Saved history did not confirm a read-only attachment",
        ));
    }
    let sources = state.upgrade_chats.entry(source.id).or_default();
    if !sources.iter().any(|previous| previous.source == *source) {
        if sources.len() >= crate::upgrade::handoff::MAX_CHATS {
            return Err(unavailable(
                "Saved history aliases exceed the bounded recovery limit",
            ));
        }
        sources.push(crate::upgrade::handoff::ChatSource {
            source: source.clone(),
            launch: source.launch.clone(),
        });
    }
    Ok(())
}

pub(in crate::host) fn release(
    host: &SharedHost,
    proof: &crate::terminal_ipc::ChatReleaseProof,
    dry_run: bool,
) -> Result<crate::terminal_ipc::ChatReleaseInspection, ProtocolError> {
    let inspection = inspect(host, proof.session.id)?;
    if inspection.proof != *proof {
        return Err(error(
            "The exact native actor or its generation changed; inspect it again",
        ));
    }
    if let Some(reason) = &inspection.release_blocked {
        return Err(unavailable(reason.clone()));
    }
    let mut reservation = None;
    let (handle, source) = {
        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
        validate_proof(proof.session.proof.as_ref(), state.proof())?;
        let (current, handle, source) = inspected_record(&state, proof.session.id)?;
        if current.proof != *proof || current.release_blocked.is_some() {
            return Err(error(
                "The exact native actor or its release eligibility changed; inspect it again",
            ));
        }
        if dry_run {
            return Ok(inspection);
        }
        Reservation::install(&mut reservation, host, &mut state, &proof.session)?;
        (handle, source)
    };
    // Repeat complete coverage with the ownership reservation held. Paging
    // admitted before the last detach may still have settled since inspection.
    super::super::account_lifecycle::ensure_idle(host)?;
    let reloaded = reload_history(&proof.session).map_err(unavailable)?;
    if !handle
        .history_is_covered_by(&reloaded)
        .map_err(unavailable)?
    {
        return Err(unavailable(
            "Reloaded provider history does not cover all resident data",
        ));
    }
    if handle
        .snapshot(None)
        .expect("unconditional history snapshot")
        .generation
        != proof.generation
    {
        return Err(error(
            "The native history generation changed; inspect it again",
        ));
    }
    // A history reader only stops paging. A completed writer has already
    // confirmed teardown and owns no provider work; close joins its worker.
    // Neither path may stop a running provider to make it eligible.
    handle.close().map_err(unavailable)?;
    // The worker is now immutable. A previously queued page must also be
    // recoverable before its cache can be dropped.
    if !handle
        .history_is_covered_by(&reloaded)
        .map_err(unavailable)?
    {
        return Err(unavailable(
            "History changed while its reader closed; resident data was preserved",
        ));
    }
    // The exclusive request gate remains held through exact capability
    // retirement and resident removal. No cached-origin submission may cross
    // this boundary; the reservation also fences actor recovery and attach.
    super::super::agent_execution::registry(host)
        .map_err(unavailable)?
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .retire_resident_release(
            &crate::agent_execution::HostedSession::Chat(source),
            release_kind(&handle),
        )
        .map_err(unavailable)?;
    let retired = {
        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
        let record = state
            .chats
            .get(&proof.session.id)
            .ok_or_else(|| error("Native history reader changed while closing"))?;
        if !record.handle.same_actor(&handle)
            || record.execution_instance != proof.execution_instance
            || !record.subscribers.is_empty()
            || record.handle.owns_work()
            || state.chat_spawning.get(&proof.session.id) != Some(&proof.session)
        {
            return Err(error(
                "The exact native history reader changed; its successor was preserved",
            ));
        }
        state.inventory_generation += 1;
        let retired = state.chats.remove(&proof.session.id);
        if let Some(notifications) = &state.notifications {
            notifications.retire(proof.session.id);
        }
        retired
    };
    // Store cleanup and final worker joining must never hold the host mutex.
    drop(retired);
    drop(reservation);
    Ok(inspection)
}
