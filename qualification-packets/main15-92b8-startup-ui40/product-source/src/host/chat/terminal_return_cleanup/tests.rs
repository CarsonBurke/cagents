use super::*;
use crate::model::{LaunchRecipe, TerminalContinuity, TerminalSession};

struct Fixture {
    _directory: tempfile::TempDir,
    host: SharedHost,
    terminal: TerminalSession,
    worker: crate::terminal::TerminalHandle,
    token: uuid::Uuid,
}

impl Fixture {
    fn new(command: &str) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let proof = HostSessionProof {
            host_session_id: "issued-return-test".into(),
            boot_id: "issued-return-boot".into(),
            host_incarnation: "issued-return-incarnation".into(),
        };
        let host = Arc::new(HostLock::new(HostState::new(proof)));
        let recipe = LaunchRecipe {
            program: "/bin/sh".into(),
            arguments: vec!["-c".into(), command.into()],
            working_directory: directory.path().into(),
            environment: BTreeMap::new(),
            unset_environment: Default::default(),
        };
        let terminal = TerminalSession {
            id: TerminalId::new(),
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
        };
        let worker = crate::terminal::TerminalHandle::spawn(
            terminal.clone(),
            crate::terminal::TerminalConfig::default(),
            |_| {},
        )
        .unwrap();
        let (terminal, token) = {
            let mut state = host.lock().unwrap();
            state.register_client(1);
            state.reserve_spawn(terminal.id).unwrap();
            let terminal = state
                .finish_spawn(1, terminal, worker.clone(), false)
                .unwrap()
                .session;
            let admission = Reservation::new(&host, &mut state, TerminalId::new()).unwrap();
            let token = admission.publish(&mut state, &terminal, &worker, None);
            drop(state);
            drop(admission);
            (terminal, token.unwrap())
        };
        Self {
            _directory: directory,
            host,
            terminal,
            worker,
            token,
        }
    }

    fn successor(&self) -> crate::terminal::TerminalHandle {
        let worker = crate::terminal::TerminalHandle::spawn(
            self.terminal.clone(),
            crate::terminal::TerminalConfig::default(),
            |_| {},
        )
        .unwrap();
        let mut state = self.host.lock().unwrap();
        state.retire_terminal(self.terminal.id);
        state.reserve_spawn(self.terminal.id).unwrap();
        state
            .finish_spawn(1, self.terminal.clone(), worker.clone(), false)
            .unwrap();
        worker
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.worker.shutdown().unwrap();
    }
}

fn acknowledge_fixture(
    fixture: &Fixture,
    id: TerminalId,
    token: Option<uuid::Uuid>,
) -> Result<(), ProtocolError> {
    let released = {
        let mut state = fixture.host.lock().unwrap();
        acknowledge(&mut state, id, token)
    }?;
    drop(released);
    Ok(())
}

#[test]
fn issued_close_preserves_same_id_same_recipe_successor() {
    let fixture = Fixture::new("exec /bin/cat");
    fixture.worker.shutdown().unwrap();
    let successor = fixture.successor();
    let failure = close(&fixture.host, fixture.terminal.id, fixture.token).unwrap_err();
    assert!(failure.retryable);
    assert!(!successor.is_completed());
    let state = fixture.host.lock().unwrap();
    assert!(
        state.terminals[&fixture.terminal.id]
            .handle
            .same_worker(&successor)
    );
    assert!(state.issued_terminal_returns[&fixture.token].pending());
    drop(state);
    successor.shutdown().unwrap();
}

#[test]
fn positive_original_close_and_lost_reply_replay_never_touch_successor() {
    let fixture = Fixture::new("exec /bin/cat");
    assert_eq!(
        close(&fixture.host, fixture.terminal.id, fixture.token).unwrap(),
        TerminalCloseResult::Closed
    );
    assert!(fixture.worker.is_completed());
    assert!(
        !fixture
            .host
            .lock()
            .unwrap()
            .terminals
            .contains_key(&fixture.terminal.id)
    );
    assert!(!fixture.host.lock().unwrap().issued_terminal_returns[&fixture.token].pending());
    let successor = fixture.successor();
    assert_eq!(
        close(&fixture.host, fixture.terminal.id, fixture.token).unwrap(),
        TerminalCloseResult::Closed
    );
    assert!(!successor.is_completed());
    assert!(
        fixture.host.lock().unwrap().terminals[&fixture.terminal.id]
            .handle
            .same_worker(&successor)
    );
    acknowledge_fixture(&fixture, fixture.terminal.id, Some(fixture.token)).unwrap();
    acknowledge_fixture(&fixture, fixture.terminal.id, Some(fixture.token)).unwrap();
    assert!(
        !fixture
            .host
            .lock()
            .unwrap()
            .issued_terminal_returns
            .contains_key(&fixture.token)
    );
    assert!(acknowledge_fixture(&fixture, TerminalId::new(), Some(fixture.token)).is_err());
    assert!(close(&fixture.host, fixture.terminal.id, fixture.token).is_err());
    assert!(!successor.is_completed());
    successor.shutdown().unwrap();
}

#[test]
fn nil_unknown_and_wrong_terminal_receipts_refuse_without_effects() {
    let fixture = Fixture::new("exec /bin/cat");
    for (id, token) in [
        (fixture.terminal.id, uuid::Uuid::nil()),
        (fixture.terminal.id, uuid::Uuid::new_v4()),
        (TerminalId::new(), fixture.token),
    ] {
        assert!(close(&fixture.host, id, token).unwrap_err().retryable);
        assert!(!fixture.worker.is_completed());
        assert!(fixture.host.lock().unwrap().issued_terminal_returns[&fixture.token].pending());
    }
}

#[test]
fn changed_native_instance_refuses_before_stopping_original_terminal() {
    let fixture = Fixture::new("exec /bin/cat");
    let (handle, _other_attachment) = Handle::fixture_remote_attachment_pair();
    let source = Session {
        id: fixture.terminal.id,
        launch: chat::Launch {
            provider: Provider::Codex,
            recipe: fixture.terminal.launch.clone(),
            session_id: Some("12345678-1234-4234-8234-123456789abc".into()),
        },
        proof: Some(fixture.host.lock().unwrap().proof().clone()),
        terminal: Some(fixture.terminal.clone()),
        owned_codex_listener: None,
        native_continuity: Default::default(),
    };
    let instance = uuid::Uuid::new_v4();
    {
        let mut state = fixture.host.lock().unwrap();
        state.chats.insert(
            source.id,
            Record {
                execution_instance: instance,
                console_recipe: None,
                paired_terminal: Some(fixture.worker.clone()),
                session: source.clone(),
                handle: handle.clone(),
                source_process: None,
                subscribers: BTreeMap::from([(1, false)]),
                observations: BTreeMap::new(),
                completion_observed: false,
                returning_terminal: false,
            },
        );
        let target = CloseTarget::from_record(&state.chats[&source.id]);
        state
            .issued_terminal_returns
            .get_mut(&fixture.token)
            .unwrap()
            .native = Some(target);
        state.chats.get_mut(&source.id).unwrap().execution_instance = uuid::Uuid::new_v4();
    }
    assert!(
        close(&fixture.host, fixture.terminal.id, fixture.token)
            .unwrap_err()
            .retryable
    );
    assert!(!fixture.worker.is_completed());
    let mut state = fixture.host.lock().unwrap();
    assert!(state.chats[&source.id].handle.same_attachment(&handle));
    assert!(state.issued_terminal_returns[&fixture.token].pending());
    let removed = state.chats.remove(&source.id);
    drop(state);
    drop(removed);
}

#[test]
fn nonce_ack_is_effect_free_and_cannot_evict_closing_or_newer_capability() {
    let fixture = Fixture::new("exec /bin/cat");
    acknowledge_fixture(&fixture, fixture.terminal.id, None).unwrap();
    assert!(
        acknowledge_fixture(&fixture, fixture.terminal.id, Some(uuid::Uuid::new_v4()))
            .unwrap_err()
            .retryable
    );
    assert!(
        acknowledge_fixture(&fixture, TerminalId::new(), Some(fixture.token))
            .unwrap_err()
            .retryable
    );
    assert!(
        fixture
            .host
            .lock()
            .unwrap()
            .issued_terminal_returns
            .contains_key(&fixture.token)
    );
    fixture
        .host
        .lock()
        .unwrap()
        .issued_terminal_returns
        .get_mut(&fixture.token)
        .unwrap()
        .closing = true;
    assert!(
        acknowledge_fixture(&fixture, fixture.terminal.id, Some(fixture.token))
            .unwrap_err()
            .retryable
    );
    assert!(
        fixture
            .host
            .lock()
            .unwrap()
            .issued_terminal_returns
            .contains_key(&fixture.token)
    );
    fixture
        .host
        .lock()
        .unwrap()
        .issued_terminal_returns
        .get_mut(&fixture.token)
        .unwrap()
        .closing = false;
    acknowledge_fixture(&fixture, fixture.terminal.id, Some(fixture.token)).unwrap();
    assert!(
        !fixture
            .host
            .lock()
            .unwrap()
            .issued_terminal_returns
            .contains_key(&fixture.token)
    );
    assert!(
        acknowledge_fixture(&fixture, TerminalId::new(), Some(fixture.token))
            .unwrap_err()
            .retryable
    );
    acknowledge_fixture(&fixture, fixture.terminal.id, Some(fixture.token)).unwrap();
    assert!(!fixture.worker.is_completed());
}

#[test]
fn natural_exit_requires_actual_original_release_and_absence_stays_unknown() {
    let fixture = Fixture::new("exit 0");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !fixture.worker.is_completed() {
        assert!(
            Instant::now() < deadline,
            "original natural exit did not settle"
        );
        thread::sleep(Duration::from_millis(10));
    }
    // The completed record still identifies the actual worker, so shutdown
    // joins that original owner before this endpoint can cache Closed.
    assert_eq!(
        close(&fixture.host, fixture.terminal.id, fixture.token).unwrap(),
        TerminalCloseResult::Closed
    );
    let absent = Fixture::new("exit 0");
    absent.worker.shutdown().unwrap();
    absent
        .host
        .lock()
        .unwrap()
        .retire_terminal(absent.terminal.id);
    assert!(
        close(&absent.host, absent.terminal.id, absent.token)
            .unwrap_err()
            .retryable
    );
    assert!(
        acknowledge_fixture(&absent, absent.terminal.id, Some(absent.token))
            .unwrap_err()
            .retryable
    );
    assert!(absent.host.lock().unwrap().issued_terminal_returns[&absent.token].pending());
}

#[test]
fn issuance_reuses_original_owner_and_capacity_refuses_without_worker_effects() {
    let fixture = Fixture::new("exec /bin/cat");
    let mut state = fixture.host.lock().unwrap();
    let admission = Reservation::new(&fixture.host, &mut state, TerminalId::new()).unwrap();
    let repeated = admission.publish(&mut state, &fixture.terminal, &fixture.worker, None);
    let count = state.issued_terminal_returns.len();
    drop(state);
    drop(admission);
    assert_eq!(repeated.unwrap(), fixture.token);
    assert_eq!(count, 1);
    let mut state = fixture.host.lock().unwrap();
    while state.terminal_return_reservations.len() < MAX_ISSUED_RETURNS - 1 {
        state
            .terminal_return_reservations
            .insert(uuid::Uuid::new_v4());
    }
    assert!(Reservation::new(&fixture.host, &mut state, TerminalId::new()).is_err());
    assert!(!fixture.worker.is_completed());
    assert!(state.issued_terminal_returns[&fixture.token].pending());
    state.terminal_return_reservations.clear();
}

#[test]
fn unwound_attempt_retains_original_custody_and_allows_explicit_original_retry() {
    let fixture = Fixture::new("exec /bin/cat");
    let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let receipt = {
            let mut state = fixture.host.lock().unwrap();
            let receipt = state.issued_terminal_returns[&fixture.token].clone();
            state
                .issued_terminal_returns
                .get_mut(&fixture.token)
                .unwrap()
                .closing = true;
            receipt
        };
        let _attempt = Attempt {
            host: fixture.host.clone(),
            token: fixture.token,
            receipt,
        };
        panic!("fixture request unwound after claiming original cleanup, before owner effect");
    }));
    assert!(unwound.is_err());
    {
        let state = fixture.host.lock().unwrap();
        let retained = &state.issued_terminal_returns[&fixture.token];
        assert!(!retained.closing);
        assert!(retained.pending());
        assert!(retained.worker.same_worker(&fixture.worker));
    }
    assert!(
        !fixture.worker.is_completed(),
        "unwind guard must not perform cleanup"
    );
    let (response, extras, disconnect) = crate::host::client::process_request(
        &fixture.host,
        1,
        ClientRequest::ChatCloseReturnedTerminal {
            terminal_id: fixture.terminal.id,
            cleanup_receipt: fixture.token,
        },
    )
    .unwrap();
    assert!(matches!(response, ServerResponse::TerminalCloseChecked {
        terminal_id, outcome: TerminalCloseResult::Closed,
    } if terminal_id == fixture.terminal.id));
    assert!(extras.is_empty());
    assert!(!disconnect);
    assert!(fixture.worker.is_completed());
}

#[test]
fn upgrade_requires_custody_for_unconfirmed_original_but_not_settled_receipts() {
    let fixture = Fixture::new("exec /bin/cat");
    {
        let state = fixture.host.lock().unwrap();
        assert!(!state.issued_terminal_returns[&fixture.token].blocks_upgrade(&state));
    }
    fixture.worker.shutdown().unwrap();
    {
        let mut state = fixture.host.lock().unwrap();
        state.retire_terminal(fixture.terminal.id);
        let receipt = &state.issued_terminal_returns[&fixture.token];
        assert!(receipt.pending());
        assert!(receipt.blocks_upgrade(&state));
        assert!(state.terminals.is_empty());
        assert!(state.chats.is_empty());
        assert!(state.chat_handoffs.is_empty());
    }

    let settled = Fixture::new("exec /bin/cat");
    assert_eq!(
        close(&settled.host, settled.terminal.id, settled.token).unwrap(),
        TerminalCloseResult::Closed,
    );
    {
        let state = settled.host.lock().unwrap();
        assert!(!state.issued_terminal_returns[&settled.token].blocks_upgrade(&state));
    }
    acknowledge_fixture(&settled, settled.terminal.id, Some(settled.token)).unwrap();
    let state = settled.host.lock().unwrap();
    assert!(state.issued_terminal_returns.is_empty());
    assert_eq!(state.acknowledged_terminal_returns.len(), 1);
}
