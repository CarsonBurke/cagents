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
                "Daemon native chat capacity reached: {} resident, {} starting, limit {}. Release explicitly verified completed chats or change the configured capacity; idle chats may still own provider work.",
                state.chats.len(),
                state.chat_spawning.len(),
                state.native_chat_capacity,
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
            if paired {
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
        let failure =
            start(&host, 1, session(), None, false, None, Default::default()).unwrap_err();
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
        let directory = tempfile::tempdir().unwrap();
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
        handle.close().unwrap();
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
}
