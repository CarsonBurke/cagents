// Actual provider construction, helper takeover and same-helper restart under
// bounded native-history pressure. This is HOST owning-contract coverage, not
// installed-provider/native UI qualification. The existing original Child /
// pidfd harness bounds this exact libtest child; outer B0 separately owns full
// descendant-family settlement. Unknown keeps the strong original owner.
fn capacity_isolated(name: &str) -> bool {
    // The owning harness retains capped logs/private runtime on both outcomes.
    owning_close_isolated(name).is_some()
}

fn capacity_receipt(path: &Path, scenario: &str, detail: serde_json::Value) {
    use std::os::unix::fs::OpenOptionsExt;
    let output = std::fs::OpenOptions::new().write(true).create_new(true)
        .mode(0o600).custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path.with_file_name(format!("capacity-{scenario}.json"))).unwrap();
    serde_json::to_writer_pretty(output, &serde_json::json!({
        "schema":"native-capacity-owning-v1", "scenario":scenario,
        "provider":"private fixture Claude", "detail":detail,
        "native_or_full_family_qualification":false
    })).unwrap();
}

struct CapacityFixture {
    host: SharedHost,
    path: std::path::PathBuf,
    proof: HostSessionProof,
    cleanup: BoundCloseCleanup,
}
impl CapacityFixture {
    fn prepare() -> Self {
        let (host, _request, path) = fixture();
        // Arm strong ownership BEFORE lease, broker, initialize or spawn.
        let cleanup = BoundCloseCleanup::reserve(&host);
        let proof = host.lock().unwrap().proof().clone();
        let paths = crate::daemon::RuntimePaths::discover().unwrap();
        cleanup.owner.lock().unwrap().lease = Some(
            crate::daemon::HostLease::acquire(&paths, &proof).unwrap());
        let broker = crate::host::notifications::Broker::start(
            &host, crate::notifications::Application::Cagents).unwrap();
        host.lock().unwrap().notifications = Some(broker.handle());
        cleanup.owner.lock().unwrap().notifications = Some(broker);
        host.lock().unwrap().enable_holder_recovery();
        Self { host, path, proof, cleanup }
    }
    fn fresh(&self) -> chat::Session {
        chat::fresh_session(Provider::Claude, TerminalId::new(),
            self.path.parent().unwrap().join("project"), &Account::system(Provider::Claude)).unwrap()
    }
    fn context(&self) -> Arc<chat::protocol::ExecutionContext> {
        Arc::new(chat::protocol::ExecutionContext::capture(
            &self.path.parent().unwrap().join("project")).unwrap())
    }
    fn capture(&mut self, id: TerminalId) -> (chat::runtime::Handle, chat::runtime::ActorIdentity, chat::Session) {
        // Keep the actual published proxy before any later fallible assertion.
        let handle = self.host.lock().unwrap().chats[&id].handle.clone();
        let identity = handle.actor_identity().expect("actual retained helper");
        self.cleanup.owner.lock().unwrap().remote = Some((handle.clone(), identity.clone(), self.proof.clone()));
        let mut source = self.host.lock().unwrap().chats[&id].session.clone();
        source.launch.session_id = handle.session_id();
        source.native_continuity = handle.native_continuity();
        let inspection = chat::runtime::Handle::inspect_remote(&identity.record, self.proof.clone()).unwrap();
        assert_eq!(inspection.identity, identity);
        assert!(inspection.source == source || inspection.previous_sources.contains(&source));
        (handle, identity, source)
    }
    fn starts(&self) -> usize {
        match std::fs::read_to_string(self.path.with_file_name("spawns")) {
            Ok(text) => text.lines().count(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
            Err(error) => panic!("cannot inspect private provider-start evidence: {error}"),
        }
    }
}

#[test]
fn native_capacity_constructor_refuses_before_adapter_initialize_or_provider_start() {
    if capacity_isolated("native_capacity_constructor_refuses_before_adapter_initialize_or_provider_start") { return; }
    let mut f = CapacityFixture::prepare();
    let source = f.fresh();
    let id = source.id;
    let context = f.context();
    let pins = f.host.lock().unwrap().native_sources.pin_capacity_for_test();
    let usage = f.host.lock().unwrap().native_sources.capacity_usage_for_test();
    assert_eq!(usage, (1024, 1024));
    let initialized = super::super::chat::adapter_initializations_for_test();
    let error = super::super::chat::start_execution(
        &f.host, EXECUTION_CLIENT, source.clone(), context.clone()).unwrap_err();
    assert!(error.message.contains("capacity is reserved"), "{error:?}");
    assert_eq!(super::super::chat::adapter_initializations_for_test(), initialized);
    assert_eq!(f.starts(), 0);
    assert_eq!(context.reconcile_cleanup().unwrap(), chat::protocol::LaunchOutcome::NoProvider);
    {
        let state = f.host.lock().unwrap();
        assert_eq!(state.native_sources.capacity_usage_for_test(), usage, "refusal debits no slot");
        assert!(!state.chats.contains_key(&id));
        assert!(!state.chat_spawning.contains_key(&id));
        assert!(!state.native_actor_startups.contains_key(&id));
        assert!(!state.issued_chat_ids.contains(&id));
    }
    drop(pins);
    let response = super::super::chat::start_execution(
        &f.host, EXECUTION_CLIENT, source, context).unwrap();
    let (handle, identity, _) = f.capture(id);
    assert!(matches!(response, crate::terminal_ipc::ServerResponse::ChatAttached { execution_instance, .. }
        if execution_instance == identity.instance));
    assert_eq!(super::super::chat::adapter_initializations_for_test(), initialized + 1);
    assert_eq!(f.starts(), 1);
    assert!(identity.record.root.is_current() && identity.record.holder.is_current());
    assert!(!handle.is_completed());
    f.cleanup.finish_result().unwrap();
    assert!(chat::runtime::Handle::retained_retirement_proved(&identity.record, &f.proof).unwrap());
    capacity_receipt(&f.path, "constructor", serde_json::json!({
        "refused_usage":usage, "refused_initializations":0, "refused_starts":0,
        "positive_initializations":1, "positive_starts":1, "exact_owner":identity,
        "positive_semantic_retirement":true
    }));
}

#[test]
fn native_capacity_lease_adoption_refuses_before_actual_helper_epoch_takeover() {
    if capacity_isolated("native_capacity_lease_adoption_refuses_before_actual_helper_epoch_takeover") { return; }
    let mut f = CapacityFixture::prepare();
    let source = f.fresh();
    let id = source.id;
    super::super::chat::start_execution(&f.host, EXECUTION_CLIENT, source, f.context()).unwrap();
    let (original, identity, source) = f.capture(id);
    let before = chat::runtime::Handle::inspect_remote(&identity.record, f.proof.clone()).unwrap();
    assert!(identity.record.root.is_current() && identity.record.holder.is_current());
    assert_eq!(f.starts(), 1);
    let mut next = f.proof.clone();
    next.host_incarnation = crate::model::HostIncarnation::new("capacity-adoption-successor");
    let mut state = crate::host::state::HostState::new(next.clone());
    state.install_model(Registry::at(&f.path).unwrap());
    state.enable_holder_recovery();
    let successor = Arc::new(crate::host::state::HostLock::new(state));
    let pins = successor.lock().unwrap().native_sources.pin_capacity_for_test();
    let usage = successor.lock().unwrap().native_sources.capacity_usage_for_test();
    assert_eq!(usage, (1024, 1024));
    // The old Host and exact proxy stay strong. Rotate only the real private
    // daemon lease; the helper's attach epoch is a separate observed capability.
    {
        let mut owner = f.cleanup.owner.lock().unwrap();
        owner.previous_host = Some(f.host.clone());
        owner.host = successor.clone();
        drop(owner.lease.take());
        owner.lease = Some(crate::daemon::HostLease::acquire(
            &crate::daemon::RuntimePaths::discover().unwrap(), &next).unwrap());
    }
    let broker = crate::host::notifications::Broker::start(
        &successor, crate::notifications::Application::Cagents).unwrap();
    successor.lock().unwrap().notifications = Some(broker.handle());
    f.cleanup.owner.lock().unwrap().notifications = Some(broker);
    let error = super::super::chat::start_execution(
        &successor, EXECUTION_CLIENT, source.clone(), f.context()).unwrap_err();
    assert!(error.message.contains("capacity is reserved"), "{error:?}");
    let refused = chat::runtime::Handle::inspect_remote(&identity.record, next.clone()).unwrap();
    assert_eq!(refused.attachment_epoch, before.attachment_epoch, "no actual helper takeover on refusal");
    assert_eq!(refused.identity, identity);
    assert_eq!(refused.source, before.source);
    assert_eq!(refused.previous_sources, before.previous_sources);
    assert!(identity.record.root.is_current() && identity.record.holder.is_current());
    assert!(f.host.lock().unwrap().chats[&id].handle.same_actor(&original));
    {
        let state = successor.lock().unwrap();
        assert_eq!(state.native_sources.capacity_usage_for_test(), usage);
        assert!(!state.chats.contains_key(&id));
        assert!(!state.chat_spawning.contains_key(&id));
    }
    assert_eq!(f.starts(), 1);
    drop(pins);
    let response = super::super::chat::start_execution(
        &successor, EXECUTION_CLIENT, source, f.context()).unwrap();
    let adopted = successor.lock().unwrap().chats[&id].handle.clone();
    f.cleanup.owner.lock().unwrap().remote = Some((adopted.clone(), identity.clone(), next.clone()));
    let after = chat::runtime::Handle::inspect_remote(&identity.record, next.clone()).unwrap();
    assert_ne!(after.attachment_epoch, before.attachment_epoch, "positive control must execute actual takeover");
    assert_eq!(adopted.actor_identity(), Some(identity.clone()));
    assert_eq!(after.identity, identity);
    assert!(matches!(response, crate::terminal_ipc::ServerResponse::ChatAttached { session, .. }
        if session.proof.as_ref() == Some(&next)));
    assert_eq!(f.starts(), 1, "adoption never constructs a replacement backend");
    assert!(identity.record.root.is_current() && identity.record.holder.is_current());
    f.cleanup.finish_result().unwrap();
    assert!(chat::runtime::Handle::retained_retirement_proved(&identity.record, &next).unwrap());
    capacity_receipt(&f.path, "adoption", serde_json::json!({
        "refused_usage":usage, "before_epoch":before.attachment_epoch,
        "refused_epoch":refused.attachment_epoch, "adopted_epoch":after.attachment_epoch,
        "exact_owner":identity, "total_starts":1, "positive_semantic_retirement":true
    }));
}

#[test]
fn native_capacity_genuine_retained_restart_refuses_without_starting_a_successor() {
    if capacity_isolated("native_capacity_genuine_retained_restart_refuses_without_starting_a_successor") { return; }
    let mut f = CapacityFixture::prepare();
    let program = f.path.parent().unwrap().join("bin/claude");
    let mut script = std::fs::read_to_string(&program).unwrap();
    // Existing guarded peer records only the real received user and emitted
    // assistant frames, with identity parsed from the provider's actual argv.
    std::fs::write(f.path.parent().unwrap().join("bin/claude-history.py"),
        include_str!("retained_close_history_peer.py")).unwrap();
    let assistant_line = script.lines().filter(|line|
        line.trim_start().starts_with("printf '{\"type\":\"assistant\""))
        .last().unwrap().to_owned();
    let index = script.rfind(&assistant_line).unwrap();
    script.replace_range(index..index + assistant_line.len(),
        &format!("{assistant_line} | python3 \"$HOME/../bin/claude-history.py\" \"$session\" \"$frame\""));
    let anchor = "      ;;\n  esac\ndone\n";
    assert_eq!(script.matches(anchor).count(), 1);
    let script = script.replacen(anchor,
        "      if [ \"$(wc -l < \"$HOME/../spawns\")\" -eq 1 ]; then exit 0; fi\n      ;;\n  esac\ndone\n", 1);
    std::fs::write(&program, script).unwrap();
    let source = f.fresh();
    let id = source.id;
    super::super::chat::start_execution(&f.host, EXECUTION_CLIENT, source, f.context()).unwrap();
    let (handle, identity, _) = f.capture(id);
    let original = identity.record.root.clone();
    handle.command(chat::Command::Prompt { text:"one exact fixture turn".into(), attachments:Vec::new(), mode:chat::PromptMode::Send }).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        handle.refresh_retained_state().unwrap();
        if handle.is_completed() && !handle.owns_work() { break; }
        assert!(Instant::now() < deadline, "real first provider did not finish");
        std::thread::sleep(Duration::from_millis(5));
    }
    let (_, _, source) = f.capture(id);
    assert!(source.launch.session_id.is_some(), "real protocol result confirms the restart identity");
    let transcript = crate::agent::claude_transcript(
        &f.path.parent().unwrap().join("home/.claude"),
        &source.launch.recipe.working_directory.canonicalize().unwrap(),
        source.launch.session_id.as_deref().unwrap()).unwrap();
    let history = std::fs::read_to_string(transcript).unwrap();
    assert_eq!(history.lines().count(), 2, "actual user/assistant frames form the real private resume history");
    assert!(history.contains("SERVICE_RESULT_1"));
    chat::protocol::create(&source.launch).expect("actual private history must satisfy the resume factory BEFORE pinning capacity");
    assert!(!original.is_current() && identity.record.holder.is_current());
    assert_eq!(f.starts(), 1);
    let pins = f.host.lock().unwrap().native_sources.pin_capacity_for_test();
    let usage = f.host.lock().unwrap().native_sources.capacity_usage_for_test();
    assert_eq!(usage, (1024, 1024));
    let before = chat::runtime::Handle::inspect_remote(&identity.record, f.proof.clone()).unwrap();
    let error = super::super::chat::process(&f.host, EXECUTION_CLIENT,
        crate::terminal_ipc::ClientRequest::ChatAttach { session:source.clone() }).unwrap_err();
    assert!(error.message.contains("already reserves"), "{error:?}");
    let refused = chat::runtime::Handle::inspect_remote(&identity.record, f.proof.clone()).unwrap();
    assert_eq!(refused.attachment_epoch, before.attachment_epoch);
    assert_eq!(refused.identity, identity);
    assert_eq!(refused.source, before.source);
    assert_eq!(refused.previous_sources, before.previous_sources);
    assert_eq!(f.host.lock().unwrap().native_sources.capacity_usage_for_test(), usage);
    assert!(f.host.lock().unwrap().chats[&id].handle.same_actor(&handle));
    assert!(handle.is_completed() && !original.is_current());
    assert!(identity.record.holder.is_current());
    assert_eq!(f.starts(), 1, "all-pinned refusal never reaches the actual helper restart spawn");
    drop(pins);
    let response = super::super::chat::process(&f.host, EXECUTION_CLIENT,
        crate::terminal_ipc::ClientRequest::ChatAttach { session:source.clone() }).unwrap();
    let crate::terminal_ipc::ServerResponse::ChatRestarted { receipt, .. } = response else {
        panic!("actual completed Attach must return its correlated restart receipt: {response:?}");
    };
    assert!(receipt.is_valid());
    assert_eq!(receipt.source(), &source);
    assert_eq!(receipt.replacement().id, id);
    assert_eq!(receipt.replacement().launch, source.launch);
    handle.refresh_retained_state().unwrap();
    let successor = handle.provider_process().expect("actual second provider published by retained helper");
    assert_ne!(successor, original);
    assert!(successor.is_current() && identity.record.holder.is_current());
    assert_eq!(handle.actor_identity(), Some(identity.clone()));
    assert!(f.host.lock().unwrap().chats[&id].handle.same_actor(&handle));
    assert_eq!(f.host.lock().unwrap().chats[&id].execution_instance, identity.instance);
    assert_eq!(receipt.replacement(), &f.capture(id).2);
    assert_eq!(f.starts(), 2);
    f.cleanup.finish_result().unwrap();
    assert!(chat::runtime::Handle::retained_retirement_proved(&identity.record, &f.proof).unwrap());
    assert!(!successor.is_current());
    capacity_receipt(&f.path, "restart", serde_json::json!({
        "refused_usage":usage, "refused_additional_starts":0,
        "original":original, "successor":successor, "exact_helper":identity,
        "total_starts":2, "positive_semantic_retirement":true
    }));
}
