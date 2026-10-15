//! Bounded, positive backend publication history. Unknown is a fence, including
//! an accepted restart whose helper reply was lost. Retirement never erases it.
use super::*;
use crate::terminal_ipc::{ChatSourceBinding, TerminalHandoffReceipt};
use std::collections::VecDeque;

const CAPACITY: usize = 1024;
#[derive(Default)]
pub(in crate::host) struct History {
    entries: BTreeMap<TerminalId, Entry>,
    order: VecDeque<TerminalId>,
}
struct Entry {
    instance: uuid::Uuid,
    unresolved: bool,
    known: Option<(chat::runtime::BackendIdentity, ChatSourceBinding)>,
}
impl History {
    /// Called BEFORE publication of every new native Record, even if its
    /// provider has not become ready or subsequently retires without attaching.
    pub(in crate::host) fn installed(&mut self, id: TerminalId, instance: uuid::Uuid) {
        if !self.entries.contains_key(&id) {
            while self.entries.len() >= CAPACITY {
                if let Some(old) = self.order.pop_front() { self.entries.remove(&old); }
            }
            self.order.push_back(id);
        }
        self.entries.insert(id, Entry { instance, unresolved: false, known: None });
    }
    /// Fence even a start which creates a backend but loses its reply before
    /// Record installation. The previous positive publication stays invalid.
    pub(in crate::host) fn mutating(&mut self, id: TerminalId) {
        if let Some(entry) = self.entries.get_mut(&id) {
            entry.known = None;
            entry.unresolved = true;
        }
    }
    pub(super) fn observed(&mut self, id: TerminalId, instance: uuid::Uuid, thread: Option<String>) -> bool {
        let Some(entry) = self.entries.get_mut(&id).filter(|e| e.instance == instance) else { return false; };
        if entry.known.as_ref().is_some_and(|(_, binding)| binding.source.launch.session_id != thread) {
            entry.known = None;
            return true;
        }
        false
    }
    fn invalidate(&mut self, id: TerminalId, instance: uuid::Uuid) -> Result<(), ProtocolError> {
        let entry = self.entries.get_mut(&id).filter(|e| e.instance == instance)
            .ok_or_else(|| unavailable("Native backend publication history is unavailable; no owner was changed"))?;
        entry.known = None;
        entry.unresolved = true;
        Ok(())
    }
    fn publish(&mut self, source: Session, instance: uuid::Uuid,
        witness: chat::runtime::BackendIdentity, resolve: bool) -> Result<ChatSourceBinding, ProtocolError> {
        let entry = self.entries.get_mut(&source.id).filter(|e| e.instance == instance)
            .ok_or_else(|| unavailable("Native backend publication history was replaced or evicted"))?;
        if entry.unresolved && !resolve {
            return Err(unavailable("The exact backend restart remains unresolved; retry its original operation before returning to a terminal"));
        }
        if let Some((previous, binding)) = &entry.known {
            if *previous == witness { return Ok(binding.clone()); }
        }
        // Even a failed restart resolved back to A mints fresh authority. An
        // earlier A intent never learns that no intervening backend existed.
        let binding = ChatSourceBinding { source, execution_instance: instance, backend: uuid::Uuid::new_v4() };
        entry.known = Some((witness, binding.clone()));
        entry.unresolved = false;
        Ok(binding)
    }
    pub(in crate::host) fn validates(&self, binding: &ChatSourceBinding) -> bool {
        self.entries.get(&binding.source.id).is_some_and(|entry| !entry.unresolved
            && entry.instance == binding.execution_instance
            && entry.known.as_ref().is_some_and(|(_, issued)| issued == binding))
    }
}

/// Must run while the caller holds the exact per-chat transition. No private
/// helper RPC or provider inspection occurs under HostLock.
pub(super) fn refresh(host: &SharedHost, id: TerminalId, resolve: bool) -> Result<ChatSourceBinding, ProtocolError> {
    let (handle, instance, proof) = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        let record = state.chats.get(&id).ok_or_else(|| unavailable("Native backend no longer resident"))?;
        (record.handle.clone(), record.execution_instance, state.proof().clone())
    };
    let witness = handle.backend_identity(&proof).map_err(unavailable)?;
    if !witness.valid() || instance.is_nil() { return Err(unavailable("Native backend evidence is incomplete")); }
    let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
    let record = state.chats.get(&id).filter(|r| r.execution_instance == instance && r.handle.same_actor(&handle))
        .ok_or_else(|| unavailable("Native actor changed while authenticating its backend"))?;
    let mut source = session_descriptor(record);
    source.launch.session_id = witness.conversation();
    validate_proof(source.proof.as_ref(), state.proof())?;
    let binding = state.native_sources.publish(source, instance, witness, resolve)?;
    state.chats.get_mut(&id).expect("same lock").source_binding = Some(binding.clone());
    Ok(binding)
}

/// Publish uncertainty BEFORE crossing a provider-mutating RPC, including
/// repeated calls inspecting an accepted but not yet acknowledged restart.
pub(super) fn restarting(host: &SharedHost, id: TerminalId, handle: &Handle) -> Result<(), ProtocolError> {
    let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
    let instance = state.chats.get(&id).filter(|r| r.handle.same_actor(handle))
        .map(|r| r.execution_instance).ok_or_else(|| unavailable("Native actor changed before backend restart"))?;
    if !state.native_sources.entries.contains_key(&id) {
        state.native_sources.installed(id, instance);
    }
    state.native_sources.invalidate(id, instance)?;
    state.chats.get_mut(&id).expect("same lock").source_binding = None;
    Ok(())
}

/// Admission proof is compared with fresh actual backend evidence while the
/// same transition guard fences restart. Never upgrades stale user intent.
pub(super) fn require(host: &SharedHost, client: ClientId, expected: &ChatSourceBinding) -> Result<(), ProtocolError> {
    {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        validate_proof(expected.source.proof.as_ref(), state.proof())?;
        let record = attached(&state, client, expected.source.id)?;
        let current = session_descriptor(record);
        if current.launch.provider != expected.source.launch.provider
            || current.launch.recipe != expected.source.launch.recipe
            || expected.source.launch.session_id.as_ref().is_some_and(|thread| current.launch.session_id.as_ref() != Some(thread)) {
            return Err(unavailable("The captured provider conversation changed; no owner was stopped"));
        }
        if record.execution_instance != expected.execution_instance || !state.native_sources.validates(expected) {
            return Err(unavailable("The captured native backend changed; no provider or console was stopped"));
        }
    }
    let current = refresh(host, expected.source.id, false)?;
    if current != *expected {
        return Err(unavailable("The captured native backend changed; no provider or console was stopped"));
    }
    Ok(())
}

/// Adds only authenticated metadata to the response produced for this request.
/// A backend not ready/inspectable grants no destructive authority.
pub(super) fn decorate(host: &SharedHost, response: &mut ServerResponse) {
    let id = match response {
        ServerResponse::ChatAttached { session, .. } => session.id,
        ServerResponse::ChatSnapshot { chat_id, .. }
        | ServerResponse::ChatUpdate { chat_id, .. } => *chat_id,
        _ => return,
    };
    let resident = host.lock().unwrap_or_else(|p| p.into_inner()).chats.contains_key(&id);
    if !resident { return; }
    let binding = refresh(host, id, false).ok();
    let state = host.lock().unwrap_or_else(|p| p.into_inner());
    let instance = state.chats.get(&id).map(|r| r.execution_instance).unwrap_or_default();
    match response {
        ServerResponse::ChatAttached { execution_instance, binding: out, .. }
        | ServerResponse::ChatSnapshot { execution_instance, binding: out, .. }
        | ServerResponse::ChatUpdate { execution_instance, binding: out, .. } => {
            *execution_instance = instance;
            *out = binding;
        }
        _ => unreachable!(),
    }
}

pub(in crate::host) fn validate_receipt(state: &HostState, receipt: &TerminalHandoffReceipt) -> Result<(), ProtocolError> {
    if receipt.source_execution_instance != receipt.source_binding.execution_instance
        || !state.native_sources.validates(&receipt.source_binding) {
        return Err(unavailable("The handoff's original native backend was superseded or its history is unavailable"));
    }
    Ok(())
}

/// Old host tests insert a real owned actor directly instead of using the
/// production publication seam. Establish that seam explicitly, then require
/// genuine child/helper evidence; no timestamp, nil ID or synthetic process.
#[cfg(test)]
pub(in crate::host) fn fixture_binding(host: &SharedHost, id: TerminalId) -> ChatSourceBinding {
    {
        let mut state = host.lock().unwrap();
        if let Some(receipt) = state.chat_handoffs.get(&id).and_then(|h| h.issued()) {
            return receipt.source_binding.clone();
        }
        let instance = state.chats[&id].execution_instance;
        assert!(!instance.is_nil(), "fixture must publish an actual actor incarnation");
        if !state.native_sources.entries.contains_key(&id) {
            state.native_sources.installed(id, instance);
        }
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match refresh(host, id, false) {
            Ok(binding) => return binding,
            Err(error) => {
                assert!(Instant::now() < deadline, "fixture must own a concrete actual backend: {}", error.message);
                thread::sleep(Duration::from_millis(2));
            }
        }
    }
}

/// Exercises production publication/admission with real runtime owners. Only
/// test construction skips launching through the installed provider registry.
#[cfg(test)]
pub(crate) struct BackendFixture {
    host: SharedHost,
    id: TerminalId,
    handle: Handle,
}
#[cfg(test)]
impl BackendFixture {
    pub(crate) fn new(source: Session, handle: Handle, instance: uuid::Uuid) -> Self {
        let mut state = HostState::new(source.proof.clone().expect("authenticated fixture"));
        state.register_client(1);
        state.native_sources.installed(source.id, instance);
        state.chats.insert(source.id, Record {
            execution_instance: instance, console_recipe: None, paired_terminal: None,
            handoff_receipt: None, source_binding: None, session: source.clone(),
            handle: handle.clone(), source_process: None,
            subscribers: BTreeMap::from([(1, false)]), observations: BTreeMap::new(),
            completion_observed: false, returning_terminal: false,
        });
        Self { host: Arc::new(HostLock::new(state)), id: source.id, handle }
    }
    pub(crate) fn capture(&self) -> Result<ChatSourceBinding, ProtocolError> { refresh(&self.host, self.id, false) }
    pub(crate) fn begin_restart(&self) { restarting(&self.host, self.id, &self.handle).unwrap(); }
    pub(crate) fn resolve(&self) -> ChatSourceBinding { refresh(&self.host, self.id, true).unwrap() }
    pub(crate) fn retire(&self) { drop(self.host.lock().unwrap().chats.remove(&self.id)); }
    pub(crate) fn validates(&self, binding: &ChatSourceBinding) -> bool {
        self.host.lock().unwrap().native_sources.validates(binding)
    }
    pub(crate) fn return_refuses(&self, binding: ChatSourceBinding) -> String {
        // Actual owning host dispatch, including its transition and attachment
        // admission. Refusal must happen before any provider/terminal teardown.
        process_inner(&self.host, 1, ClientRequest::ChatReturnTerminal { chat_id: self.id, expected: binding }, None)
            .err().expect("stale captured intent must refuse").message
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::protocol::{Adapter, ProcessSpec, Step};
    struct Idle;
    impl Adapter for Idle {
        fn initialize(&mut self) -> Result<Step, String> { Ok(Step::default()) }
        fn receive(&mut self, _: serde_json::Value) -> Result<Step, String> { Ok(Step::default()) }
        fn command(&mut self, _: chat::Command) -> Result<Step, String> { Ok(Step::default()) }
    }
    struct Owner(Handle);
    impl Drop for Owner { fn drop(&mut self) { let _ = self.0.close(); } }
    fn actor(source: &Session) -> Owner {
        let spec = ProcessSpec::from(&source.launch.recipe);
        let mut initial = Step::default();
        let store = chat::runtime::preflight(&spec, &mut initial).unwrap();
        let handle = Handle::start(spec, Box::new(Idle), initial, store, None, |_| {}).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while handle.provider_process().is_none() { assert!(Instant::now() < deadline); thread::yield_now(); }
        Owner(handle)
    }
    fn source(path: &std::path::Path) -> Session {
        Session { id: TerminalId::new(), proof: Some(HostSessionProof {
            host_session_id: "binding-test".into(), boot_id: crate::daemon::read_boot_id().unwrap(), host_incarnation: "binding-test".into(),
        }), launch: chat::Launch { provider: Provider::Omp, session_id: None,
            recipe: crate::model::LaunchRecipe { program: "/bin/cat".into(), arguments: vec![], working_directory: path.into(), environment: BTreeMap::new(), unset_environment: Default::default() } },
            terminal: None, native_continuity: Default::default(), owned_codex_listener: None }
    }
    #[test]
    fn retired_same_id_actor_cannot_resurrect_an_earlier_backend_binding() {
        let directory = tempfile::tempdir_in("/var/tmp").unwrap();
        let source = source(directory.path());
        let first = actor(&source);
        let fixture = BackendFixture::new(source.clone(), first.0.clone(), uuid::Uuid::new_v4());
        let a = fixture.capture().unwrap();
        let second = actor(&source);
        let b = uuid::Uuid::new_v4();
        {
            let mut state = fixture.host.lock().unwrap();
            state.native_sources.installed(source.id, b);
            let record = state.chats.get_mut(&source.id).unwrap();
            record.execution_instance = b;
            record.handle = second.0.clone();
            record.source_binding = None;
        }
        let binding_b = fixture.capture().unwrap();
        assert_ne!(a.backend, binding_b.backend);
        assert!(!fixture.validates(&a));
        assert!(fixture.return_refuses(a.clone()).contains("captured native backend changed"));
        assert!(!second.0.is_completed(), "stale A consent cannot close real B");
        second.0.close().unwrap();
        fixture.retire();
        assert!(!fixture.validates(&a), "B's retirement never recreates A authority");
        assert!(fixture.validates(&binding_b), "exact latest history survives retirement");
    }
    #[test]
    fn ambiguous_restart_fences_same_record_and_eviction_never_grants_authority() {
        let directory = tempfile::tempdir_in("/var/tmp").unwrap();
        let source = source(directory.path());
        let owner = actor(&source);
        let fixture = BackendFixture::new(source.clone(), owner.0.clone(), uuid::Uuid::new_v4());
        let a = fixture.capture().unwrap();
        fixture.begin_restart();
        assert!(fixture.capture().is_err());
        assert!(fixture.return_refuses(a.clone()).contains("captured native backend changed"));
        assert!(!owner.0.is_completed());
        let resolved = fixture.resolve();
        assert_ne!(resolved.backend, a.backend, "even a proved failed restart cannot revive an old user intent");
        let mut state = fixture.host.lock().unwrap();
        for _ in 0..CAPACITY { state.native_sources.installed(TerminalId::new(), uuid::Uuid::new_v4()); }
        assert!(!state.native_sources.validates(&resolved));
        assert!(state.native_sources.entries.len() <= CAPACITY);
    }
}
