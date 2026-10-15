// Owning HOST contracts for the bound-close positive disposition. These use
// the real retained Claude facade/helper and protocol peer, not a synthetic
// BackendFixture or fabricated Pending/claim/child. Native product is separate.
struct BoundCloseCleanup {
    host: SharedHost,
    attempted: bool,
    notifications: Option<crate::host::notifications::Broker>,
    lease: Option<crate::daemon::HostLease>,
}
impl BoundCloseCleanup {
    fn finish(&mut self) {
        if self.attempted { return; }
        self.attempted = true;
        if let Err(error) = super::super::finish_chat_shutdown(&self.host) {
            eprintln!("Exact bound-close fixture cleanup is unconfirmed: {error}");
            // No timeout/Err is settlement. The bounded original Child and
            // outer common guardian retain the remaining cleanup obligation.
            if !std::thread::panicking() { panic!("Unconfirmed bound-close cleanup: {error}"); }
        }
    }
}
impl Drop for BoundCloseCleanup { fn drop(&mut self) { self.finish(); } }
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
        let mut cleanup = BoundCloseCleanup {host: host.clone(), attempted:false,
            notifications:None, lease:None};
        let proof = host.lock().unwrap().proof().clone();
        let paths = crate::daemon::RuntimePaths::discover().unwrap();
        cleanup.lease = Some(crate::daemon::HostLease::acquire(&paths, &proof).unwrap());
        cleanup.notifications = Some(crate::host::notifications::Broker::start(&host,
            crate::notifications::Application::Cagents).unwrap());
        host.lock().unwrap().notifications = Some(cleanup.notifications.as_ref().unwrap().handle());
        host.lock().unwrap().enable_holder_recovery();
        let accepted = submit(&host, &Authority::default(), request).unwrap();
        let run = settled(&host, accepted.run.id);
        assert_eq!(run.state, State::Succeeded, "{run:?}");
        let agent = registry(&host).unwrap().lock().unwrap().agent(accepted.agent.id).unwrap();
        let id = agent.session.as_ref().unwrap().id();
        let handle = host.lock().unwrap().chats[&id].handle.clone();
        let identity = handle.actor_identity().expect("genuine retained Remote owner");
        let binding = super::super::chat::source_binding::owning_binding_for_test(&host, id).unwrap();
        let inspection = chat::runtime::Handle::inspect_remote(&identity.record, proof.clone()).unwrap();
        assert_eq!(inspection.identity, identity);
        assert!(inspection.source == binding.source || inspection.previous_sources.contains(&binding.source));
        assert!(binding.source.terminal.is_none());
        assert!(!handle.is_completed(), "owning peer must stay live for the actual claim");
        assert!(identity.record.root.is_current());
        assert_eq!(host.lock().unwrap().chats[&id].execution_instance, binding.execution_instance);
        Self {host,path,agent,run,handle,identity,binding,proof,cleanup}
    }
    fn claim(&self, lose: Option<Box<dyn FnOnce(uuid::Uuid) + Send>>) -> Result<(), crate::terminal_ipc::ProtocolError> {
        super::super::chat::source_binding::owning_claim_for_test(&self.host, &self.binding, lose)
    }
    fn phase(&self) -> Option<(Option<uuid::Uuid>,bool)> {
        super::super::chat::source_binding::pending_claim_for_test(&self.host, &self.binding)
    }
    fn close_session(&self) -> Result<crate::terminal_ipc::ServerResponse,crate::terminal_ipc::ProtocolError> {
        super::super::chat::process(&self.host, 1, crate::terminal_ipc::ClientRequest::ChatCloseSession {
            session:self.binding.source.clone()})
    }
    fn assert_closed(&self) {
        assert!(self.phase().is_none(), "positive proof must retire the exact pending phase");
        let state = self.host.lock().unwrap();
        assert!(!state.chats.contains_key(&self.binding.source.id));
        assert!(!state.native_closing.contains_key(&self.binding.source.id));
        drop(state);
        assert!(chat::runtime::Handle::retained_retirement_proved(&self.identity.record, &self.proof).unwrap());
        assert!(!self.identity.record.root.is_current());
        assert!(!self.identity.record.holder.is_current());
        let store = registry(&self.host).unwrap(); let store = store.lock().unwrap();
        assert!(store.agent(self.agent.id).unwrap().closed);
        assert_eq!(store.agent_run(self.run.id).unwrap(), self.run);
        assert_eq!(std::fs::read_to_string(self.path.with_file_name("spawns")).unwrap().lines().count(), 1);
    }
}
impl Drop for BoundCloseFixture { fn drop(&mut self) { self.cleanup.finish(); } }

#[test]
fn combined_close_retained_lifecycle_pending_claim_closes_via_chat_close() {
    if owning_close_isolated("combined_close_retained_lifecycle_pending_claim_closes_via_chat_close").is_some() {return;}
    let f = BoundCloseFixture::start();
    super::super::chat::process(&f.host, 1, crate::terminal_ipc::ClientRequest::ChatAttach {
        session:f.binding.source.clone()}).unwrap();
    f.claim(None).unwrap();
    assert!(f.phase().is_some_and(|(token,closed)| token.is_some() && !closed));
    let result = super::super::chat::process(&f.host, 1, crate::terminal_ipc::ClientRequest::ChatClose {
        chat_id:f.binding.source.id}).unwrap();
    assert!(matches!(result,crate::terminal_ipc::ServerResponse::ChatClosed {..}));
    f.assert_closed();
}

#[test]
fn combined_close_retained_lifecycle_lost_host_claim_result_recovers_original_token() {
    if owning_close_isolated("combined_close_retained_lifecycle_lost_host_claim_result_recovers_original_token").is_some() {return;}
    let f = BoundCloseFixture::start();
    let observed = Arc::new(Mutex::new(None));
    let saved = observed.clone();
    assert!(f.claim(Some(Box::new(move |token| {*saved.lock().unwrap()=Some(token);}))).unwrap_err().message.contains("lost"));
    assert!(observed.lock().unwrap().is_some());
    assert_eq!(f.phase(),Some((None,false)));
    let host = f.host.clone(); let binding = f.binding.clone(); let expected = *observed.lock().unwrap();
    super::super::chat::fail_verification_for_test(f.binding.clone(), move || {
        let phase = super::super::chat::source_binding::pending_claim_for_test(&host,&binding);
        assert_eq!(phase,Some((expected,true)), "retry must recover the SAME authenticated helper token");
        Ok(())
    });
    assert!(matches!(f.close_session().unwrap(),crate::terminal_ipc::ServerResponse::ChatClosed {..}));
    f.assert_closed();
}

#[test]
fn combined_close_retained_lifecycle_final_verification_failure_keeps_exact_owner_for_retry() {
    if owning_close_isolated("combined_close_retained_lifecycle_final_verification_failure_keeps_exact_owner_for_retry").is_some() {return;}
    let f = BoundCloseFixture::start(); f.claim(None).unwrap();
    let original = f.phase().unwrap().0;
    let host = f.host.clone(); let id = f.binding.source.id; let handle = f.handle.clone();
    super::super::chat::fail_verification_for_test(f.binding.clone(), move || {
        let state = host.lock().unwrap();
        assert!(state.chats[&id].handle.same_actor(&handle));
        assert!(state.native_closing.contains_key(&id));
        Err("Test final receipt verification is unconfirmed".into())
    });
    assert!(f.close_session().unwrap_err().message.contains("verification is unconfirmed"));
    assert_eq!(f.phase(),Some((original,true)));
    {
        let state=f.host.lock().unwrap();
        assert!(state.chats[&id].handle.same_actor(&f.handle));
        assert!(state.native_closing.contains_key(&id));
    }
    // Durable model close may ALREADY have committed. This is a retained
    // disposition retry, never a claim that the provider/model rolled back.
    assert_eq!(registry(&f.host).unwrap().lock().unwrap().agent_run(f.run.id).unwrap(),f.run);
    assert!(matches!(f.close_session().unwrap(),crate::terminal_ipc::ServerResponse::ChatClosed {..}));
    f.assert_closed();
}
