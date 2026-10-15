// Owning HOST contracts for the bound-close positive disposition. These use
// the real retained Claude facade/helper and protocol peer, not a synthetic
// BackendFixture or fabricated Pending/claim/child. Native product is separate.
// One strong owner slot per exact owning subprocess. UNKNOWN retains its
// original Host, lease, Broker and captured proxy until subprocess exit. The
// outer originalChild/common guardian still must settle the entire family.
static BOUND_CLOSE_OWNER: Mutex<Option<Arc<Mutex<BoundCloseOwner>>>> = Mutex::new(None);
struct BoundCloseOwner {
    host: SharedHost,
    // A lease-adoption fixture keeps the original Host strongly owned too.
    previous_host: Option<SharedHost>,
    notifications: Option<crate::host::notifications::Broker>,
    lease: Option<crate::daemon::HostLease>,
    remote: Option<(chat::runtime::Handle, chat::runtime::ActorIdentity, HostSessionProof)>,
}
struct BoundCloseCleanup {
    owner: Arc<Mutex<BoundCloseOwner>>,
    outcome: Option<Result<(), String>>,
}
impl BoundCloseCleanup {
    fn reserve(host: &SharedHost) -> Self {
        let mut retained = BOUND_CLOSE_OWNER.lock().unwrap_or_else(|p| p.into_inner());
        assert!(retained.is_none(), "a prior exact bound-close owner remains unconfirmed; no new owner may start");
        let owner = Arc::new(Mutex::new(BoundCloseOwner {
            host: host.clone(), previous_host: None, notifications: None, lease: None, remote: None,
        }));
        *retained = Some(owner.clone());
        Self { owner, outcome: None }
    }
    fn finish_result(&mut self) -> Result<(), String> {
        if let Some(outcome) = &self.outcome { return outcome.clone(); }
        let (host, remote) = {
            let context = self.owner.lock().unwrap_or_else(|p| p.into_inner());
            (context.host.clone(), context.remote.clone())
        };
        let mut failures = Vec::new();
        if let Err(error) = super::super::finish_chat_shutdown(&host) {
            failures.push(error);
        }
        if let Some((_owner, identity, proof)) = &remote {
            match chat::runtime::Handle::retained_retirement_proved(&identity.record, proof) {
                Ok(true) => {}
                Ok(false) => failures.push("Exact retained helper retirement is unconfirmed".into()),
                Err(error) => failures.push(error),
            }
        }
        let outcome = if failures.is_empty() { Ok(()) } else { Err(failures.join("; ")) };
        self.outcome = Some(outcome.clone());
        if outcome.is_ok() {
            let retired = {
                let mut retained = BOUND_CLOSE_OWNER.lock().unwrap_or_else(|p| p.into_inner());
                assert!(retained.as_ref().is_some_and(|owner| Arc::ptr_eq(owner, &self.owner)));
                retained.take()
            };
            drop(retired);
        }
        outcome
    }
    fn finish(&mut self) {
        if let Err(error) = self.finish_result() {
            eprintln!("Exact bound-close cleanup UNKNOWN; strong original context retained until subprocess exit; outer family owner required: {error}");
            if !std::thread::panicking() { panic!("Unconfirmed bound-close cleanup: {error}"); }
        }
    }
}
impl Drop for BoundCloseCleanup {
    fn drop(&mut self) { self.finish(); }
}
struct BoundCloseFixture {
    host: SharedHost,
    path: std::path::PathBuf,
    agent: Agent,
    run: Run,
    handle: chat::runtime::Handle,
    identity: chat::runtime::ActorIdentity,
    binding: crate::terminal_ipc::ChatSourceBinding,
    proof: HostSessionProof,
    cleanup: BoundCloseCleanup,
}
impl BoundCloseFixture {
    fn start() -> Self {
        let (host, request, path) = fixture();
        // Armed BEFORE lease/notification/dispatch or a fallible assertion.
        // Keep both resources alive through every constructor panic/close.
        let cleanup = BoundCloseCleanup::reserve(&host);
        let proof = host.lock().unwrap().proof().clone();
        let paths = crate::daemon::RuntimePaths::discover().unwrap();
        cleanup.owner.lock().unwrap().lease = Some(crate::daemon::HostLease::acquire(&paths, &proof).unwrap());
        cleanup.owner.lock().unwrap().notifications = Some(
            crate::host::notifications::Broker::start(
                &host,
                crate::notifications::Application::Cagents,
            )
            .unwrap(),
        );
        host.lock().unwrap().notifications = Some(cleanup.owner.lock().unwrap().notifications.as_ref().unwrap().handle());
        host.lock().unwrap().enable_holder_recovery();
        let accepted = submit(&host, &Authority::default(), request).unwrap();
        let run = settled(&host, accepted.run.id);
        assert_eq!(run.state, State::Succeeded, "{run:?}");
        let agent = registry(&host)
            .unwrap()
            .lock()
            .unwrap()
            .agent(accepted.agent.id)
            .unwrap();
        let id = agent.session.as_ref().unwrap().id();
        let handle = host.lock().unwrap().chats[&id].handle.clone();
        let identity = handle
            .actor_identity()
            .expect("genuine retained Remote owner");
        cleanup.owner.lock().unwrap().remote = Some((handle.clone(), identity.clone(), proof.clone()));
        let binding =
            super::super::chat::source_binding::owning_binding_for_test(&host, id).unwrap();
        let inspection =
            chat::runtime::Handle::inspect_remote(&identity.record, proof.clone()).unwrap();
        assert_eq!(inspection.identity, identity);
        assert!(
            inspection.source == binding.source
                || inspection.previous_sources.contains(&binding.source)
        );
        assert!(binding.source.terminal.is_none());
        assert!(
            !handle.is_completed(),
            "owning peer must stay live for the actual claim"
        );
        assert!(identity.record.root.is_current());
        assert_eq!(
            host.lock().unwrap().chats[&id].execution_instance,
            binding.execution_instance
        );
        Self {
            host,
            path,
            agent,
            run,
            handle,
            identity,
            binding,
            proof,
            cleanup,
        }
    }
    fn claim(
        &self,
        lose: Option<Box<dyn FnOnce(uuid::Uuid) + Send>>,
    ) -> Result<(), crate::terminal_ipc::ProtocolError> {
        super::super::chat::source_binding::owning_claim_for_test(&self.host, &self.binding, lose)
    }
    fn phase(&self) -> Option<(Option<uuid::Uuid>, bool)> {
        super::super::chat::source_binding::pending_claim_for_test(&self.host, &self.binding)
    }
    fn close_session(
        &self,
    ) -> Result<crate::terminal_ipc::ServerResponse, crate::terminal_ipc::ProtocolError> {
        super::super::chat::process(
            &self.host,
            1,
            crate::terminal_ipc::ClientRequest::ChatCloseSession {
                session: self.binding.source.clone(),
            },
        )
    }
    fn assert_closed(&self) {
        assert!(
            self.phase().is_none(),
            "positive proof must retire the exact pending phase"
        );
        let state = self.host.lock().unwrap();
        assert!(!state.chats.contains_key(&self.binding.source.id));
        assert!(!state.native_closing.contains_key(&self.binding.source.id));
        drop(state);
        assert!(
            chat::runtime::Handle::retained_retirement_proved(&self.identity.record, &self.proof)
                .unwrap()
        );
        assert!(!self.identity.record.root.is_current());
        assert!(!self.identity.record.holder.is_current());
        let store = registry(&self.host).unwrap();
        let store = store.lock().unwrap();
        assert!(store.agent(self.agent.id).unwrap().closed);
        assert_eq!(store.agent_run(self.run.id).unwrap(), self.run);
        assert_eq!(
            std::fs::read_to_string(self.path.with_file_name("spawns"))
                .unwrap()
                .lines()
                .count(),
            1
        );
    }
}
impl Drop for BoundCloseFixture {
    fn drop(&mut self) {
        self.cleanup.finish();
    }
}

#[test]
fn combined_close_retained_lifecycle_pending_claim_closes_via_chat_close() {
    if owning_close_isolated(
        "combined_close_retained_lifecycle_pending_claim_closes_via_chat_close",
    )
    .is_some()
    {
        return;
    }
    let f = BoundCloseFixture::start();
    super::super::chat::process(
        &f.host,
        1,
        crate::terminal_ipc::ClientRequest::ChatAttach {
            session: f.binding.source.clone(),
        },
    )
    .unwrap();
    f.claim(None).unwrap();
    assert!(
        f.phase()
            .is_some_and(|(token, closed)| token.is_some() && !closed)
    );
    let result = super::super::chat::process(
        &f.host,
        1,
        crate::terminal_ipc::ClientRequest::ChatClose {
            chat_id: f.binding.source.id,
        },
    )
    .unwrap();
    assert!(matches!(
        result,
        crate::terminal_ipc::ServerResponse::ChatClosed { .. }
    ));
    f.assert_closed();
}

#[test]
fn combined_close_retained_lifecycle_lost_host_claim_result_recovers_original_token() {
    if owning_close_isolated(
        "combined_close_retained_lifecycle_lost_host_claim_result_recovers_original_token",
    )
    .is_some()
    {
        return;
    }
    let f = BoundCloseFixture::start();
    let observed = Arc::new(Mutex::new(None));
    let saved = observed.clone();
    assert!(
        f.claim(Some(Box::new(move |token| {
            *saved.lock().unwrap() = Some(token);
        })))
        .unwrap_err()
        .message
        .contains("lost")
    );
    assert!(observed.lock().unwrap().is_some());
    assert_eq!(f.phase(), Some((None, false)));
    let host = f.host.clone();
    let binding = f.binding.clone();
    let expected = *observed.lock().unwrap();
    super::super::chat::fail_verification_for_test(f.binding.clone(), move || {
        let phase = super::super::chat::source_binding::pending_claim_for_test(&host, &binding);
        assert_eq!(
            phase,
            Some((expected, true)),
            "retry must recover the SAME authenticated helper token"
        );
        Ok(())
    });
    assert!(matches!(
        f.close_session().unwrap(),
        crate::terminal_ipc::ServerResponse::ChatClosed { .. }
    ));
    f.assert_closed();
}

#[test]
fn combined_close_retained_lifecycle_final_verification_failure_keeps_exact_owner_for_retry() {
    if owning_close_isolated(
        "combined_close_retained_lifecycle_final_verification_failure_keeps_exact_owner_for_retry",
    )
    .is_some()
    {
        return;
    }
    let f = BoundCloseFixture::start();
    f.claim(None).unwrap();
    let original = f.phase().unwrap().0;
    let host = f.host.clone();
    let id = f.binding.source.id;
    let handle = f.handle.clone();
    super::super::chat::fail_verification_for_test(f.binding.clone(), move || {
        let state = host.lock().unwrap();
        assert!(state.chats[&id].handle.same_actor(&handle));
        assert!(state.native_closing.contains_key(&id));
        Err("Test final receipt verification is unconfirmed".into())
    });
    assert!(
        f.close_session()
            .unwrap_err()
            .message
            .contains("verification is unconfirmed")
    );
    assert_eq!(f.phase(), Some((original, true)));
    {
        let state = f.host.lock().unwrap();
        assert!(state.chats[&id].handle.same_actor(&f.handle));
        assert!(state.native_closing.contains_key(&id));
    }
    // The real public replacement producer must not install a same-ID backend
    // while this exact pending operation still owns its source reservation.
    // This proves replacement refusal, not an invented reachable successor.
    let replacement = super::super::chat::process(
        &f.host,
        1,
        crate::terminal_ipc::ClientRequest::ChatAttach {
            session: f.binding.source.clone(),
        },
    );
    assert!(
        replacement.is_err(),
        "pending native cleanup cannot start a replacement"
    );
    assert_eq!(f.phase(), Some((original, true)));
    assert!(
        f.host.lock().unwrap().chats[&id]
            .handle
            .same_actor(&f.handle)
    );
    assert_eq!(
        std::fs::read_to_string(f.path.with_file_name("spawns"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    // Durable model close may ALREADY have committed. This is a retained
    // disposition retry, never a claim that the provider/model rolled back.
    assert_eq!(
        registry(&f.host)
            .unwrap()
            .lock()
            .unwrap()
            .agent_run(f.run.id)
            .unwrap(),
        f.run
    );
    assert!(matches!(
        f.close_session().unwrap(),
        crate::terminal_ipc::ServerResponse::ChatClosed { .. }
    ));
    f.assert_closed();
}

#[test]
fn combined_close_retained_lifecycle_attempted_return_stays_unknown_until_exact_retry() {
    if owning_close_isolated(
        "combined_close_retained_lifecycle_attempted_return_stays_unknown_until_exact_retry",
    )
    .is_some()
    {
        return;
    }
    let f = BoundCloseFixture::start();
    super::super::chat::process(
        &f.host,
        1,
        crate::terminal_ipc::ClientRequest::ChatAttach {
            session: f.binding.source.clone(),
        },
    )
    .unwrap();
    f.host
        .lock()
        .unwrap()
        .native_sources
        .lose_next_return_start_reply_for_test();
    let mut saved_attempt = None;
    crate::terminal::TerminalHandle::with_blocked_holder_cleanup_for_test(|| {
        let error = super::super::chat::process(
            &f.host,
            1,
            crate::terminal_ipc::ClientRequest::ChatReturnTerminal {
                chat_id: f.binding.source.id,
                expected: f.binding.clone(),
            },
        )
        .unwrap_err();
        assert!(
            error.message.contains("exited before startup completed"),
            "{error:?}"
        );
        let attempt = f
            .host
            .lock()
            .unwrap()
            .native_sources
            .return_attempt_for_test(f.binding.source.id)
            .expect("real attempted target, not an assigned phase");
        assert_eq!(
            attempt.disposition(),
            crate::terminal::TerminalStartDisposition::Unknown
        );
        let holder = attempt
            .holder_record()
            .expect("actual exact returned worker holder");
        assert_eq!(holder.terminal, attempt.terminal_id());
        let phase = f.phase().expect("original pending phase retained");
        assert!(phase.0.is_some() && phase.1);
        let error = f.close_session().unwrap_err();
        assert!(
            error
                .message
                .contains("attempted or paired terminal ownership"),
            "{error:?}"
        );
        assert_eq!(f.phase(), Some(phase));
        assert_eq!(
            attempt.disposition(),
            crate::terminal::TerminalStartDisposition::Unknown
        );
        assert!(
            f.host.lock().unwrap().chats[&f.binding.source.id]
                .handle
                .same_actor(&f.handle)
        );
        saved_attempt = Some(attempt);
    });
    // Cleanup/retry must reconcile the actual attempt's positive family proof,
    // never dispose Unknown from the close refusal or a missing record.
    let attempt = saved_attempt.unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if attempt.retry_cleanup() == crate::terminal::TerminalStartDisposition::FamilySettled {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "original attempted worker cleanup stayed unconfirmed"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    let response = super::super::chat::process(
        &f.host,
        1,
        crate::terminal_ipc::ClientRequest::ChatReturnTerminal {
            chat_id: f.binding.source.id,
            expected: f.binding.clone(),
        },
    )
    .unwrap();
    let crate::terminal_ipc::ServerResponse::ChatTerminal { receipt, .. } = response else {
        panic!("exact return receipt required");
    };
    let returned = f.host.lock().unwrap().terminals[&receipt.terminal.session.id]
        .handle
        .clone();
    struct CloseReturned(crate::terminal::TerminalHandle);
    impl Drop for CloseReturned {
        fn drop(&mut self) {
            let _ = self.0.shutdown();
        }
    }
    let _returned_cleanup = CloseReturned(returned.clone());
    assert_eq!(receipt.terminal.session.id, attempt.terminal_id());
    assert_ne!(
        Some(receipt.terminal.execution_instance),
        attempt.worker_instance()
    );
    assert!(f.phase().is_none());
    f.host
        .lock()
        .unwrap()
        .validate_terminal_handoff_receipt(&receipt)
        .unwrap();
    // The actual Return caller finishes semantic retirement BEFORE spawning
    // its replacement. Do not issue another retire RPC to manufacture this.
    assert!(
        chat::runtime::Handle::retained_retirement_proved(&f.identity.record, &f.proof).unwrap()
    );
    assert!(!f.identity.record.root.is_current() && !f.identity.record.holder.is_current());
    returned.shutdown().unwrap();
}

#[test]
fn combined_close_retained_lifecycle_real_created_pair_stays_held_until_exact_console_retry() {
    if owning_close_isolated(
        "combined_close_retained_lifecycle_real_created_pair_stays_held_until_exact_console_retry",
    )
    .is_some()
    {
        return;
    }
    let (host, mut request, path) = fixture();
    let mut cleanup = BoundCloseCleanup::reserve(&host);
    let root = path.parent().unwrap();
    {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
        let marker = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(root.join("combined-close-owner.json"))
            .unwrap();
        serde_json::to_writer(marker,&serde_json::json!({"root":root.canonicalize().unwrap(),"uid":std::fs::metadata(root).unwrap().uid()})).unwrap();
    }
    let program = root.join("bin/codex");
    std::fs::write(
        &program,
        include_str!("../../../tests/fixtures/combined-close-codex.py"),
    )
    .unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
    let proof = host.lock().unwrap().proof().clone();
    cleanup.owner.lock().unwrap().lease = Some(
        crate::daemon::HostLease::acquire(
            &crate::daemon::RuntimePaths::discover().unwrap(),
            &proof,
        )
        .unwrap(),
    );
    cleanup.owner.lock().unwrap().notifications = Some(
        crate::host::notifications::Broker::start(
            &host,
            crate::notifications::Application::Cagents,
        )
        .unwrap(),
    );
    host.lock().unwrap().notifications = Some(cleanup.owner.lock().unwrap().notifications.as_ref().unwrap().handle());
    host.lock().unwrap().enable_holder_recovery();
    let Destination::New(spec) = &mut request.destination else {
        unreachable!()
    };
    spec.provider = Provider::Codex;
    spec.model = Some("fixture-model".into());
    spec.reasoning_effort = Some("low".into());
    let accepted = submit(&host, &Authority::default(), request).unwrap();
    let run = settled(&host, accepted.run.id);
    assert_eq!(run.state, State::Succeeded, "{run:?}");
    let agent = registry(&host)
        .unwrap()
        .lock()
        .unwrap()
        .agent(accepted.agent.id)
        .unwrap();
    let id = agent.session.as_ref().unwrap().id();
    let handle = host.lock().unwrap().chats[&id].handle.clone();
    assert!(handle.actor_identity().is_none());
    assert!(
        handle.provider_process().is_none(),
        "genuine Local Codex childNone"
    );
    let binding = super::super::chat::source_binding::owning_binding_for_test(&host, id).unwrap();
    super::super::chat::process(
        &host,
        1,
        crate::terminal_ipc::ClientRequest::ChatAttach {
            session: binding.source.clone(),
        },
    )
    .unwrap();
    let recipe = crate::model::LaunchRecipe {
        program: "/bin/cat".into(),
        arguments: vec![],
        working_directory: root.join("project"),
        environment: Default::default(),
        unset_environment: Default::default(),
    };
    // Actual shared production pairing body after the already-validated recipe
    // boundary; /bin/cat proves ownership/publication, not installed CLI UX.
    let interrupted = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = super::super::chat::owning_console_for_test(
            &host,
            1,
            binding.clone(),
            recipe.clone(),
            || {
                panic!("test interruption after actual helper/pair source publication");
            },
        );
    }));
    let panic = interrupted.expect_err("the exact scheduled publication interruption must run");
    let message = panic
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| panic.downcast_ref::<String>().map(String::as_str));
    assert_eq!(
        message,
        Some("test interruption after actual helper/pair source publication")
    );
    let phase = super::super::chat::source_binding::pending_claim_for_test(&host, &binding)
        .expect("real pending pair operation");
    assert!(phase.0.is_some() && !phase.1);
    let pair = host.lock().unwrap().chats[&id]
        .paired_terminal
        .as_ref()
        .unwrap()
        .clone();
    assert!(pair.holder_identity().unwrap().root.is_current());
    let error = super::super::chat::process(
        &host,
        1,
        crate::terminal_ipc::ClientRequest::ChatClose { chat_id: id },
    )
    .unwrap_err();
    assert!(
        error
            .message
            .contains("attempted or paired terminal ownership"),
        "{error:?}"
    );
    assert_eq!(
        super::super::chat::source_binding::pending_claim_for_test(&host, &binding),
        Some(phase)
    );
    assert!(!handle.is_completed());
    assert!(pair.holder_identity().unwrap().root.is_current());
    assert_eq!(
        registry(&host)
            .unwrap()
            .lock()
            .unwrap()
            .agent_run(run.id)
            .unwrap(),
        run
    );
    let response =
        super::super::chat::owning_console_for_test(&host, 1, binding.clone(), recipe, || {})
            .unwrap();
    let crate::terminal_ipc::ServerResponse::ChatTerminal { receipt, .. } = response else {
        panic!("original pair receipt required");
    };
    assert_eq!(
        receipt.terminal.execution_instance,
        pair.execution_instance()
    );
    assert!(super::super::chat::source_binding::pending_claim_for_test(&host, &binding).is_none());
    assert!(
        host.lock().unwrap().chats[&id]
            .paired_terminal
            .as_ref()
            .unwrap()
            .same_worker(&pair)
    );
    assert_eq!(
        std::fs::read_to_string(root.join("codex-processes.jsonl"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    pair.shutdown().unwrap();
    cleanup.finish();
}
