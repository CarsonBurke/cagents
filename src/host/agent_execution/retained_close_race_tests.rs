// HOST owning fixtures only. The first real private Claude backend exits cleanly
// after its completed turn; the same retained helper restarts a second backend.
// No injected Actor.child/completed flags or guessed ownership metadata.
struct RetainedCloseRaceFixture {
    host: SharedHost,
    path: std::path::PathBuf,
    agent: Agent,
    run: Run,
    source: chat::Session,
    handle: chat::runtime::Handle,
    identity: chat::runtime::ActorIdentity,
    original: crate::daemon::holder_registry::Process,
    _lease: crate::daemon::HostLease,
    _notifications: crate::host::notifications::Broker,
}
impl RetainedCloseRaceFixture {
    fn start() -> Self { Self::start_with_schedule(false) }
    fn scheduled() -> Self { Self::start_with_schedule(true) }
    fn start_with_schedule(scheduled: bool) -> Self {
        let (host, request, path) = fixture();
        let root = path.parent().unwrap();
        let program = root.join("bin/claude");
        let mut script = std::fs::read_to_string(&program).unwrap();
        let gate_anchor = "while IFS= read -r frame; do";
        assert_eq!(script.matches(gate_anchor).count(), 1);
        script = script.replacen(gate_anchor, r#"if [ "$(wc -l < "$HOME/../spawns")" -gt 1 ]; then
  printf 'waiting\n' > "$HOME/../retained-restart-waiting"
  while [ ! -f "$HOME/../retained-restart-release" ]; do sleep 0.01; done
fi
while IFS= read -r frame; do"#, 1);
        let exit_anchor = "      ;;\n  esac\ndone\n";
        assert_eq!(script.matches(exit_anchor).count(), 1);
        script = script.replacen(exit_anchor, "      if [ \"$(wc -l < \"$HOME/../spawns\")\" -eq 1 ]; then exit 0; fi\n      ;;\n  esac\ndone\n", 1);
        std::fs::write(&program, script).unwrap();
        let proof = host.lock().unwrap().proof().clone();
        let paths = crate::daemon::RuntimePaths::discover().unwrap();
        let lease = crate::daemon::HostLease::acquire(&paths, &proof).unwrap();
        let notifications = crate::host::notifications::Broker::start(&host,
            crate::notifications::Application::Cagents).unwrap();
        host.lock().unwrap().notifications = Some(notifications.handle());
        host.lock().unwrap().enable_holder_recovery();
        let accepted = if scheduled {
            let automation = automation_fixture(&host, &request);
            let accepted = claim_fixture(&host, &automation);
            dispatch_accepted(&host, accepted.clone()).unwrap();
            accepted
        } else {
            submit(&host, &Authority::default(), request).unwrap()
        };
        let run = settled(&host, accepted.run.id);
        assert_eq!(run.state, State::Succeeded, "{run:?}");
        let agent = registry(&host).unwrap().lock().unwrap().agent(accepted.agent.id).unwrap();
        let id = agent.session.as_ref().unwrap().id();
        let handle = host.lock().unwrap().chats[&id].handle.clone();
        let identity = handle.actor_identity().expect("genuine retained Remote Claude fixture");
        let original = identity.record.root.clone();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            handle.refresh_retained_state().unwrap();
            if handle.is_completed() && !handle.owns_work() { break; }
            assert!(Instant::now() < deadline, "cleanly exited provider did not publish completed/non-owning state");
            std::thread::sleep(Duration::from_millis(5));
        }
        // Capture the CURRENT hosted descriptor with the same resident-only
        // projection as chat::session_descriptor, not inspect's raw pre-install
        // source. Installation publishes RetainedClaudeActor separately.
        let mut source = host.lock().unwrap().chats[&id].session.clone();
        source.launch.session_id = handle.session_id();
        source.native_continuity = handle.native_continuity();
        assert!(matches!(source.native_continuity, chat::NativeContinuity::RetainedClaudeActor {..}));
        let inspected = chat::runtime::Handle::inspect_remote(&identity.record, proof).unwrap();
        assert!(inspected.source == source || inspected.previous_sources.contains(&source),
            "current hosted descriptor must be a full authenticated helper alias");
        assert!(source.terminal.is_none(), "no deferred matched-CLI/pair restart dependency is substituted");
        assert!(identity.record.holder.is_current());
        assert!(!original.is_current());
        assert_eq!(std::fs::read_to_string(root.join("spawns")).unwrap().lines().count(), 1);
        let agent = registry(&host).unwrap().lock().unwrap().agent(agent.id).unwrap();
        Self {host, path, agent, run, source, handle, identity, original, _lease:lease, _notifications:notifications}
    }
    fn release_restart(&self) {
        std::fs::write(self.path.parent().unwrap().join("retained-restart-release"), b"release owned fixture backend").unwrap();
    }
    fn spawns(&self) -> usize {
        std::fs::read_to_string(self.path.parent().unwrap().join("spawns")).unwrap().lines().count()
    }
}
impl Drop for RetainedCloseRaceFixture {
    fn drop(&mut self) {
        // The original retained facade/helper remains the sole cleanup target;
        // a failed test never discovers a process by name or numeric PID scan.
        let _ = std::fs::write(self.path.parent().unwrap().join("retained-restart-release"), b"fixture cleanup");
        let _ = self.handle.close_for_resume();
        let _ = self.handle.finish_retained_close();
    }
}

#[test]
fn combined_close_retained_lifecycle_reservation_excludes_real_attach_restart_before_adoption() {
    if owning_close_isolated("combined_close_retained_lifecycle_reservation_excludes_real_attach_restart_before_adoption").is_some() { return; }
    let fixture = RetainedCloseRaceFixture::start();
    let store = registry(&fixture.host).unwrap();
    let (entered, entering) = std::sync::mpsc::channel();
    let (release, released) = std::sync::mpsc::channel();
    crate::host::chat::after_retained_close_reservation(move || {
        entered.send(()).unwrap();
        released.recv_timeout(Duration::from_secs(10)).unwrap();
    });
    let host = fixture.host.clone(); let source = fixture.source.clone();
    let closing = std::thread::spawn(move || crate::host::chat::close_retained_native(&host, &source));
    let reserved = entering.recv_timeout(Duration::from_secs(2));
    let (sent, returned) = std::sync::mpsc::channel();
    let host = fixture.host.clone(); let source = fixture.source.clone();
    let attaching = std::thread::spawn(move || {
        sent.send(crate::host::client::process_request(&host, EXECUTION_CLIENT,
            ClientRequest::ChatAttach {session:source})).unwrap();
    });
    let attach = returned.recv_timeout(Duration::from_secs(2));
    // Release every private scheduling gate before assertions and joins. The
    // outer harness still owns the original subprocess through its deadline.
    let _ = release.send(()); fixture.release_restart();
    let closed = closing.join().unwrap(); attaching.join().unwrap();
    assert!(reserved.is_ok(), "ordinary close never reached its pre-adoption reservation");
    let error = attach.expect("ChatAttach must promptly refuse the held lifecycle claim").unwrap_err();
    assert!(error.message.contains("lifecycle transition"), "{error:?}");
    closed.unwrap();
    assert_eq!(fixture.spawns(), 1, "a paused close must not restart a backend before adoption");
    assert!(!fixture.identity.record.holder.is_current());
    assert!(store.lock().unwrap().agent(fixture.agent.id).unwrap().closed);
    assert_eq!(store.lock().unwrap().agent_run(fixture.run.id).unwrap(), fixture.run);
}

#[test]
fn combined_close_retained_lifecycle_refuses_while_real_restart_owns_claim_and_preserves_successor() {
    if owning_close_isolated("combined_close_retained_lifecycle_refuses_while_real_restart_owns_claim_and_preserves_successor").is_some() { return; }
    let fixture = RetainedCloseRaceFixture::start();
    let store = registry(&fixture.host).unwrap();
    let host = fixture.host.clone(); let source = fixture.source.clone();
    let attaching = std::thread::spawn(move || crate::host::client::process_request(&host, EXECUTION_CLIENT,
        ClientRequest::ChatAttach {session:source}));
    let marker = fixture.path.parent().unwrap().join("retained-restart-waiting");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !marker.exists() && Instant::now() < deadline { std::thread::sleep(Duration::from_millis(5)); }
    let reached = marker.exists();
    let refused = crate::host::chat::close_retained_native(&fixture.host, &fixture.source);
    fixture.release_restart();
    let restarted = attaching.join().unwrap();
    assert!(reached, "actual second backend must be held before its initialization reply");
    assert!(refused.unwrap_err().contains("lifecycle transition"));
    let (response, _, _) = restarted.unwrap();
    assert!(matches!(response, ServerResponse::ChatAttached {..}));
    fixture.handle.refresh_retained_state().unwrap();
    let successor = fixture.handle.provider_process().expect("actual restarted backend process");
    assert_ne!(successor, fixture.original); assert!(successor.is_current());
    assert_eq!(fixture.handle.actor_identity().unwrap(), fixture.identity);
    assert_eq!(fixture.spawns(), 2);
    let restarted_agent = store.lock().unwrap().agent(fixture.agent.id).unwrap();
    assert!(!restarted_agent.closed);
    assert_eq!(restarted_agent.id, fixture.agent.id);
    assert_eq!(restarted_agent.origin, fixture.agent.origin);
    assert_eq!(restarted_agent.spec, fixture.agent.spec);
    // Once restart is complete, old cleanup must still not stop a provider
    // which now owns work; shared logical identity/recipe alone grants nothing.
    let stale = crate::host::chat::close_retained_native(&fixture.host, &fixture.source).unwrap_err();
    assert!(stale.contains("still owns work") || stale.contains("does not match"), "{stale}");
    assert!(successor.is_current()); assert!(fixture.identity.record.holder.is_current());
    assert!(!fixture.host.lock().unwrap().native_closing.contains_key(&fixture.source.id));
    assert_eq!(store.lock().unwrap().agent(fixture.agent.id).unwrap(), restarted_agent);
    assert_eq!(store.lock().unwrap().agent_run(fixture.run.id).unwrap(), fixture.run);
    assert_eq!(fixture.spawns(), 2);
}

#[test]
fn combined_close_retained_lifecycle_ordinary_receipt_lookup_keeps_host_census_responsive() {
    if owning_close_isolated("combined_close_retained_lifecycle_ordinary_receipt_lookup_keeps_host_census_responsive").is_some() { return; }
    let fixture = RetainedCloseRaceFixture::start();
    let store = registry(&fixture.host).unwrap();
    let (entered, entering) = std::sync::mpsc::channel();
    let (release, released) = std::sync::mpsc::channel();
    crate::host::chat::before_ordinary_close_receipt(move || {
        entered.send(()).unwrap();
        released.recv_timeout(Duration::from_secs(10)).unwrap();
    });
    let host = fixture.host.clone(); let id = fixture.source.id;
    let closing = std::thread::spawn(move || crate::host::client::process_request(&host, EXECUTION_CLIENT,
        ClientRequest::ChatClose {chat_id:id}));
    let reached = entering.recv_timeout(Duration::from_secs(2));
    let host = fixture.host.clone(); let (sent, census) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {sent.send(host.lock().unwrap().chats.len()).unwrap();});
    let responsive = census.recv_timeout(Duration::from_millis(500));
    let _ = release.send(()); fixture.release_restart();
    reader.join().unwrap(); let result = closing.join().unwrap();
    assert!(reached.is_ok(), "ordinary public close did not reach actual native receipt lookup");
    assert!(responsive.is_ok(), "receipt discovery retained HostState during filesystem work");
    let (reply, _, _) = result.unwrap();
    assert!(matches!(reply, ServerResponse::ChatClosed {..}));
    assert!(!fixture.identity.record.holder.is_current());
    assert_eq!(fixture.spawns(), 1);
    assert!(store.lock().unwrap().agent(fixture.agent.id).unwrap().closed);
    assert_eq!(store.lock().unwrap().agent_run(fixture.run.id).unwrap(), fixture.run);
}


#[test]
fn combined_close_retained_lifecycle_reconciliation_defers_to_initializing_restart() {
    if owning_close_isolated("combined_close_retained_lifecycle_reconciliation_defers_to_initializing_restart").is_some() { return; }
    let fixture = RetainedCloseRaceFixture::scheduled();
    assert!(matches!(fixture.agent.origin, Origin::Automation {..}));
    let store = registry(&fixture.host).unwrap();
    let (entered, entering) = std::sync::mpsc::channel();
    let (release, released) = std::sync::mpsc::channel();
    before_reconciliation_transition(move || {
        entered.send(()).unwrap();
        released.recv_timeout(Duration::from_secs(10)).unwrap();
    });
    let host = fixture.host.clone(); let run = fixture.run.clone();
    let reconciling = std::thread::spawn(move || reconcile_agent_run(&host, &run, true));
    let reached = entering.recv_timeout(Duration::from_secs(2));
    // Actual public ChatAttach creates B in the same helper. Its private
    // protocol initialization is held before readiness, with the production
    // lifecycle claim still retained. No untracked Prompt bypasses Agent gate.
    let host = fixture.host.clone(); let source = fixture.source.clone();
    let attaching = std::thread::spawn(move || crate::host::client::process_request(&host, EXECUTION_CLIENT,
        ClientRequest::ChatAttach {session:source}));
    let marker = fixture.path.parent().unwrap().join("retained-restart-waiting");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !marker.exists() && Instant::now() < deadline {std::thread::sleep(Duration::from_millis(5));}
    let initializing = marker.exists();
    let before_refusal = store.lock().unwrap().agent(fixture.agent.id).unwrap();
    let _ = release.send(());
    let result = reconciling.join().unwrap();
    let after_refusal = store.lock().unwrap().agent(fixture.agent.id).unwrap();
    fixture.release_restart();
    let restarted = attaching.join().unwrap();
    assert!(reached.is_ok()); assert!(initializing, "real restarted B must be held before readiness");
    let error = result.unwrap_err();
    assert!(error.contains("lifecycle transition"), "automatic cleanup must defer to initializing B: {error}");
    assert_eq!(before_refusal, after_refusal);
    let (response, _, _) = restarted.unwrap();
    assert!(matches!(response, ServerResponse::ChatAttached {..}));
    fixture.handle.refresh_retained_state().unwrap();
    let successor = fixture.handle.provider_process().expect("real initialized backend B");
    assert_ne!(successor, fixture.original); assert!(successor.is_current());
    assert_eq!(fixture.handle.actor_identity().unwrap(), fixture.identity);
    assert!(fixture.identity.record.holder.is_current());
    assert_eq!(fixture.spawns(), 2);
    let current = store.lock().unwrap().agent(fixture.agent.id).unwrap();
    assert_eq!(current.origin, fixture.agent.origin); assert!(!current.closed);
    assert_eq!(current.spec, fixture.agent.spec);
    assert_eq!(store.lock().unwrap().agent_run(fixture.run.id).unwrap(), fixture.run);
    assert!(!fixture.host.lock().unwrap().native_closing.contains_key(&fixture.source.id));
    // Once a later B is fully ready and idle, a NEW eligibility observation
    // may qualify. Backend replacement alone is never a blanket refusal token.
}

#[test]
fn combined_close_retained_lifecycle_reconciliation_holds_claim_across_completed_eligibility_gap() {
    if owning_close_isolated("combined_close_retained_lifecycle_reconciliation_holds_claim_across_completed_eligibility_gap").is_some() { return; }
    let fixture = RetainedCloseRaceFixture::scheduled();
    assert!(matches!(fixture.agent.origin, Origin::Automation {..}));
    let store = registry(&fixture.host).unwrap();
    let (entered, entering) = std::sync::mpsc::channel();
    let (release, released) = std::sync::mpsc::channel();
    after_reconciliation_eligibility(move || {
        entered.send(()).unwrap();
        released.recv_timeout(Duration::from_secs(10)).unwrap();
    });
    let host = fixture.host.clone(); let run = fixture.run.clone();
    let reconciling = std::thread::spawn(move || reconcile_agent_run(&host, &run, true));
    let reached = entering.recv_timeout(Duration::from_secs(2));
    let (sent, returned) = std::sync::mpsc::channel();
    let host = fixture.host.clone(); let source = fixture.source.clone();
    let attaching = std::thread::spawn(move || {sent.send(crate::host::client::process_request(&host, EXECUTION_CLIENT,
        ClientRequest::ChatAttach {session:source})).unwrap();});
    let attach = returned.recv_timeout(Duration::from_secs(2));
    let _ = release.send(()); fixture.release_restart();
    let closed = reconciling.join().unwrap(); attaching.join().unwrap();
    assert!(reached.is_ok(), "reconciliation must reach its actual completed eligibility observation before close effects");
    let error = attach.expect("restart must promptly refuse the reconciliation lifecycle claim").unwrap_err();
    assert!(error.message.contains("lifecycle transition"), "{error:?}");
    assert_eq!(closed.unwrap(), Reconciliation::Closed);
    assert!(!fixture.identity.record.holder.is_current());
    assert_eq!(fixture.spawns(), 1);
    let closed_agent = store.lock().unwrap().agent(fixture.agent.id).unwrap();
    assert!(closed_agent.closed); assert_eq!(closed_agent.origin, fixture.agent.origin);
    assert_eq!(store.lock().unwrap().agent_run(fixture.run.id).unwrap(), fixture.run);
}
