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
    resource_epoch: uuid::Uuid,
    unresolved: bool,
    learned_from: Option<ChatSourceBinding>,
    known: Option<(
        chat::runtime::BackendIdentity,
        ChatSourceBinding,
        Option<uuid::Uuid>,
    )>,
}
impl History {
    /// Called BEFORE publication of every new native Record, even if its
    /// provider has not become ready or subsequently retires without attaching.
    pub(in crate::host) fn installed(&mut self, id: TerminalId, instance: uuid::Uuid) {
        if !self.entries.contains_key(&id) {
            while self.entries.len() >= CAPACITY {
                if let Some(old) = self.order.pop_front() {
                    self.entries.remove(&old);
                }
            }
            self.order.push_back(id);
        }
        self.entries.insert(
            id,
            Entry {
                instance,
                resource_epoch: uuid::Uuid::new_v4(),
                unresolved: false,
                learned_from: None,
                known: None,
            },
        );
    }
    /// Positively retain the native-publication boundary of a returned resource
    /// even if it has never hosted a native actor. Issuance alone may create
    /// this stamp; validation only looks it up and unknown/evicted refuses.
    pub(super) fn retain_resource_epoch(&mut self, id: TerminalId) -> uuid::Uuid {
        if !self.entries.contains_key(&id) {
            self.installed(id, uuid::Uuid::nil());
        }
        self.entries[&id].resource_epoch
    }
    pub(super) fn resource_epoch(&self, id: TerminalId) -> Option<uuid::Uuid> {
        self.entries.get(&id).map(|entry| entry.resource_epoch)
    }

    /// Fence even a start which creates a backend but loses its reply before
    /// Record installation. The previous positive publication stays invalid.
    pub(in crate::host) fn mutating(&mut self, id: TerminalId) {
        self.retain_resource_epoch(id);
        if let Some(entry) = self.entries.get_mut(&id) {
            entry.resource_epoch = uuid::Uuid::new_v4();
            entry.known = None;
            entry.learned_from = None;
            entry.unresolved = true;
        }
    }
    pub(super) fn observed(
        &mut self,
        id: TerminalId,
        instance: uuid::Uuid,
        thread: Option<String>,
        revision: Option<u64>,
    ) -> bool {
        let Some(entry) = self.entries.get_mut(&id).filter(|e| e.instance == instance) else {
            return false;
        };
        let Some((witness, binding, _)) = &entry.known else {
            return false;
        };
        let changed = binding.source.launch.session_id != thread
            || revision != Some(witness.conversation_revision());
        if !changed {
            return false;
        }
        // Exactly one first confirmation may be learned under fresh admission.
        // Any missing revision, reset, second identity or same-final-thread ABA
        // revokes old receipts immediately at producer notification.
        let first = binding.source.launch.session_id.is_none()
            && witness.conversation_revision().checked_add(1) == revision;
        if !first {
            entry.known = None;
            entry.learned_from = None;
        }
        true
    }
    fn invalidate(&mut self, id: TerminalId, instance: uuid::Uuid) -> Result<(), ProtocolError> {
        let entry = self
            .entries
            .get_mut(&id)
            .filter(|e| e.instance == instance)
            .ok_or_else(|| {
                unavailable(
                    "Native backend publication history is unavailable; no owner was changed",
                )
            })?;
        entry.resource_epoch = uuid::Uuid::new_v4();
        entry.known = None;
        entry.learned_from = None;
        entry.unresolved = true;
        Ok(())
    }
    fn publish(
        &mut self,
        source: Session,
        instance: uuid::Uuid,
        witness: chat::runtime::BackendIdentity,
        paired: Option<uuid::Uuid>,
        resolve: bool,
    ) -> Result<ChatSourceBinding, ProtocolError> {
        let entry = self
            .entries
            .get_mut(&source.id)
            .filter(|e| e.instance == instance)
            .ok_or_else(|| {
                unavailable("Native backend publication history was replaced or evicted")
            })?;
        if entry.unresolved && !resolve {
            return Err(unavailable(
                "The exact backend restart remains unresolved; retry its original operation before returning to a terminal",
            ));
        }
        if let Some((previous, binding, previous_pair)) = &entry.known {
            if *previous == witness && binding.source == source && *previous_pair == paired {
                return Ok(binding.clone());
            }
        }
        entry.learned_from = entry
            .known
            .as_ref()
            .and_then(|(previous, binding, previous_pair)| {
                (previous.first_confirmation(&witness)
                    && *previous_pair == paired
                    && first_thread_learning(&binding.source, &source))
                .then(|| binding.clone())
            });
        // Even a failed restart resolved back to A mints fresh authority. An
        // earlier A intent never learns that no intervening backend existed.
        let binding = ChatSourceBinding {
            source,
            execution_instance: instance,
            backend: uuid::Uuid::new_v4(),
        };
        entry.known = Some((witness, binding.clone(), paired));
        entry.unresolved = false;
        Ok(binding)
    }
    fn authorizes_intent(&self, binding: &ChatSourceBinding) -> bool {
        self.validates(binding)
            || self.entries.get(&binding.source.id).is_some_and(|entry| {
                !entry.unresolved
                    && entry.instance == binding.execution_instance
                    && entry.known.is_some()
                    && entry.learned_from.as_ref() == Some(binding)
            })
    }
    pub(in crate::host) fn validates(&self, binding: &ChatSourceBinding) -> bool {
        self.entries.get(&binding.source.id).is_some_and(|entry| {
            !entry.unresolved
                && entry.instance == binding.execution_instance
                && entry
                    .known
                    .as_ref()
                    .is_some_and(|(_, issued, _)| issued == binding)
        })
    }
}

fn first_thread_learning(before: &Session, after: &Session) -> bool {
    if before.launch.session_id.is_some() || after.launch.session_id.is_none() {
        return false;
    }
    let mut promoted = before.clone();
    promoted.launch.session_id = after.launch.session_id.clone();
    promoted.native_continuity = after.native_continuity.clone();
    promoted == *after
}

/// Must run while the caller holds the exact per-chat transition. No private
/// helper RPC or provider inspection occurs under HostLock.
pub(super) fn refresh(
    host: &SharedHost,
    id: TerminalId,
    resolve: bool,
) -> Result<ChatSourceBinding, ProtocolError> {
    let (handle, instance, proof) = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        let record = state
            .chats
            .get(&id)
            .ok_or_else(|| unavailable("Native backend no longer resident"))?;
        (
            record.handle.clone(),
            record.execution_instance,
            state.proof().clone(),
        )
    };
    let witness = handle.backend_identity(&proof).map_err(unavailable)?;
    if !witness.valid() || instance.is_nil() {
        return Err(unavailable("Native backend evidence is incomplete"));
    }
    let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
    let record = state
        .chats
        .get(&id)
        .filter(|r| r.execution_instance == instance && r.handle.same_actor(&handle))
        .ok_or_else(|| unavailable("Native actor changed while authenticating its backend"))?;
    let mut source = session_descriptor(record);
    source.launch.session_id = witness.conversation();
    validate_proof(source.proof.as_ref(), state.proof())?;
    let paired = record
        .paired_terminal
        .as_ref()
        .map(|worker| worker.execution_instance());
    let binding = state
        .native_sources
        .publish(source, instance, witness, paired, resolve)?;
    state.chats.get_mut(&id).expect("same lock").source_binding = Some(binding.clone());
    Ok(binding)
}

/// Publish uncertainty BEFORE crossing a provider-mutating RPC, including
/// repeated calls inspecting an accepted but not yet acknowledged restart.
pub(super) fn restarting(
    host: &SharedHost,
    id: TerminalId,
    handle: &Handle,
) -> Result<(), ProtocolError> {
    let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
    let instance = state
        .chats
        .get(&id)
        .filter(|r| r.handle.same_actor(handle))
        .map(|r| r.execution_instance)
        .ok_or_else(|| unavailable("Native actor changed before backend restart"))?;
    if !state.native_sources.entries.contains_key(&id) {
        state.native_sources.installed(id, instance);
    }
    state.native_sources.invalidate(id, instance)?;
    state.chats.get_mut(&id).expect("same lock").source_binding = None;
    Ok(())
}

/// Invalidate the old publication at every actual pair mutation, including
/// pair B which publishes and retires before the next UI observation. Repeating
/// the same publication is harmless; a cleared/replaced pair never revives A.
pub(in crate::host) fn pair_changed(state: &mut HostState, id: TerminalId) {
    let Some(record) = state.chats.get(&id) else {
        return;
    };
    let paired = record
        .paired_terminal
        .as_ref()
        .map(|worker| worker.execution_instance());
    let terminal = record.session.terminal.clone();
    let instance = record.execution_instance;
    let Some(entry) = state
        .native_sources
        .entries
        .get_mut(&id)
        .filter(|entry| entry.instance == instance)
    else {
        return;
    };
    if entry.known.as_ref().is_some_and(|(_, binding, old_pair)| {
        *old_pair != paired || binding.source.terminal != terminal
    }) {
        entry.resource_epoch = uuid::Uuid::new_v4();
        entry.known = None;
        entry.learned_from = None;
        state.chats.get_mut(&id).expect("same lock").source_binding = None;
    }
}

/// Private continuation proof captured under the lifecycle claim BEFORE the
/// handoff's own pair mutation. It is never supplied by a client.
pub(super) struct Continuation {
    instance: uuid::Uuid,
    witness: chat::runtime::BackendIdentity,
}
pub(super) fn continuation(
    host: &SharedHost,
    binding: &ChatSourceBinding,
) -> Result<Continuation, ProtocolError> {
    let state = host.lock().unwrap_or_else(|p| p.into_inner());
    if !state.native_sources.validates(binding) {
        return Err(unavailable(
            "The source changed before its own pair transition",
        ));
    }
    let witness = state.native_sources.entries[&binding.source.id]
        .known
        .as_ref()
        .expect("validated publication")
        .0
        .clone();
    Ok(Continuation {
        instance: binding.execution_instance,
        witness,
    })
}

/// The operation's own paired source may change, but its actual backend and
/// confirmed conversation must remain exactly the one authenticated beforehand.
pub(super) fn complete_pair(
    host: &SharedHost,
    id: TerminalId,
    previous: &Continuation,
) -> Result<ChatSourceBinding, ProtocolError> {
    let binding = refresh(host, id, false)?;
    let state = host.lock().unwrap_or_else(|p| p.into_inner());
    let known = state
        .native_sources
        .entries
        .get(&id)
        .and_then(|entry| entry.known.as_ref());
    if binding.execution_instance != previous.instance
        || known.is_none_or(|(witness, _, _)| witness != &previous.witness)
    {
        return Err(unavailable(
            "The actual provider backend changed during its terminal pairing; ownership was retained",
        ));
    }
    Ok(binding)
}

/// Admission compares fresh actual evidence under the same transition claim.
/// An unknown thread may learn its first authenticated identity on that exact
/// backend/pair; a known identity, changed pair or intervening restart never may.
pub(super) fn require(
    host: &SharedHost,
    client: ClientId,
    expected: &ChatSourceBinding,
) -> Result<ChatSourceBinding, ProtocolError> {
    let previous = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        validate_proof(expected.source.proof.as_ref(), state.proof())?;
        let record = attached(&state, client, expected.source.id)?;
        let current = session_descriptor(record);
        if current.launch.provider != expected.source.launch.provider
            || current.launch.recipe != expected.source.launch.recipe
            || expected
                .source
                .launch
                .session_id
                .as_ref()
                .is_some_and(|thread| current.launch.session_id.as_ref() != Some(thread))
        {
            return Err(unavailable(
                "The captured provider conversation changed; no owner was stopped",
            ));
        }
        if record.execution_instance != expected.execution_instance
            || !state.native_sources.authorizes_intent(expected)
        {
            return Err(unavailable(
                "The captured native backend changed; no provider or console was stopped",
            ));
        }
        state.native_sources.entries[&expected.source.id]
            .known
            .as_ref()
            .expect("validated publication")
            .clone()
    };
    let current = refresh(host, expected.source.id, false)?;
    if current == *expected {
        return Ok(current);
    }
    let state = host.lock().unwrap_or_else(|p| p.into_inner());
    let learned = state
        .native_sources
        .entries
        .get(&expected.source.id)
        .is_some_and(|entry| {
            entry.learned_from.as_ref() == Some(expected)
                || entry.known.as_ref().is_some_and(|(witness, _, pair)| {
                    previous.0.first_confirmation(witness) && previous.2 == *pair
                })
        });
    let mut promoted = expected.source.clone();
    promoted.launch.session_id = current.source.launch.session_id.clone();
    promoted.native_continuity = current.source.native_continuity.clone();
    if expected.source.launch.session_id.is_none()
        && current.source.launch.session_id.is_some()
        && learned
        && promoted == current.source
    {
        return Ok(current);
    }
    Err(unavailable(
        "The captured native backend changed; no provider or console was stopped",
    ))
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
    let resident = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .chats
        .contains_key(&id);
    if !resident {
        return;
    }
    let binding = refresh(host, id, false).ok();
    let state = host.lock().unwrap_or_else(|p| p.into_inner());
    let instance = state
        .chats
        .get(&id)
        .map(|r| r.execution_instance)
        .unwrap_or_default();
    match response {
        ServerResponse::ChatAttached {
            execution_instance,
            binding: out,
            ..
        }
        | ServerResponse::ChatSnapshot {
            execution_instance,
            binding: out,
            ..
        }
        | ServerResponse::ChatUpdate {
            execution_instance,
            binding: out,
            ..
        } => {
            *execution_instance = instance;
            *out = binding;
        }
        _ => unreachable!(),
    }
}

pub(in crate::host) fn validate_receipt(
    state: &HostState,
    receipt: &TerminalHandoffReceipt,
) -> Result<(), ProtocolError> {
    if receipt.terminal_native_epoch.is_nil()
        || state
            .native_sources
            .resource_epoch(receipt.terminal.session.id)
            != Some(receipt.terminal_native_epoch)
        || state.native_actor_startups.contains_key(&receipt.source.id)
        || state
            .native_actor_startups
            .contains_key(&receipt.terminal.session.id)
        || receipt.source_execution_instance != receipt.source_binding.execution_instance
        || !state.native_sources.validates(
            receipt
                .retained_binding
                .as_ref()
                .unwrap_or(&receipt.source_binding),
        )
    {
        return Err(unavailable(
            "The handoff's original native backend was superseded or its history is unavailable",
        ));
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
        assert!(
            !instance.is_nil(),
            "fixture must publish an actual actor incarnation"
        );
        if !state.native_sources.entries.contains_key(&id) {
            state.native_sources.installed(id, instance);
        }
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match refresh(host, id, false) {
            Ok(binding) => return binding,
            Err(error) => {
                assert!(
                    Instant::now() < deadline,
                    "fixture must own a concrete actual backend: {}",
                    error.message
                );
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
        state.chats.insert(
            source.id,
            Record {
                execution_instance: instance,
                console_recipe: None,
                paired_terminal: None,
                handoff_receipt: None,
                source_binding: None,
                session: source.clone(),
                handle: handle.clone(),
                source_process: None,
                subscribers: BTreeMap::from([(1, false)]),
                observations: BTreeMap::new(),
                completion_observed: false,
                returning_terminal: false,
            },
        );
        Self {
            host: Arc::new(HostLock::new(state)),
            id: source.id,
            handle,
        }
    }
    pub(crate) fn capture(&self) -> Result<ChatSourceBinding, ProtocolError> {
        refresh(&self.host, self.id, false)
    }
    pub(crate) fn begin_restart(&self) {
        restarting(&self.host, self.id, &self.handle).unwrap();
    }
    pub(crate) fn resolve(&self) -> ChatSourceBinding {
        refresh(&self.host, self.id, true).unwrap()
    }
    pub(crate) fn retire(&self) {
        drop(self.host.lock().unwrap().chats.remove(&self.id));
    }
    pub(crate) fn validates(&self, binding: &ChatSourceBinding) -> bool {
        self.host.lock().unwrap().native_sources.validates(binding)
    }
    pub(crate) fn return_refuses(&self, binding: ChatSourceBinding) -> String {
        // Actual owning host dispatch, including its transition and attachment
        // admission. Refusal must happen before any provider/terminal teardown.
        process_inner(
            &self.host,
            1,
            ClientRequest::ChatReturnTerminal {
                chat_id: self.id,
                expected: binding,
            },
            None,
        )
        .err()
        .expect("stale captured intent must refuse")
        .message
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::protocol::{Adapter, ProcessSpec, Step};
    struct Idle;
    impl Adapter for Idle {
        fn initialize(&mut self) -> Result<Step, String> {
            Ok(Step::default())
        }
        fn receive(&mut self, _: serde_json::Value) -> Result<Step, String> {
            Ok(Step::default())
        }
        fn command(&mut self, _: chat::Command) -> Result<Step, String> {
            Ok(Step::default())
        }
    }
    struct Owner(Handle);
    impl Drop for Owner {
        fn drop(&mut self) {
            let _ = self.0.close();
        }
    }
    fn actor(source: &Session) -> Owner {
        actor_with(source, Box::new(Idle))
    }
    fn actor_with(source: &Session, adapter: Box<dyn Adapter>) -> Owner {
        actor_with_notify(source, adapter, |_| {})
    }
    fn actor_with_notify(
        source: &Session,
        adapter: Box<dyn Adapter>,
        notify: impl Fn(Update) + Send + Sync + 'static,
    ) -> Owner {
        let spec = ProcessSpec::from(&source.launch.recipe);
        let mut initial = Step::default();
        let store = chat::runtime::preflight(&spec, &mut initial).unwrap();
        let handle = Handle::start(spec, adapter, initial, store, None, notify).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while handle.provider_process().is_none() {
            assert!(Instant::now() < deadline);
            thread::yield_now();
        }
        Owner(handle)
    }
    fn source(path: &std::path::Path) -> Session {
        Session {
            id: TerminalId::new(),
            proof: Some(HostSessionProof {
                host_session_id: "binding-test".into(),
                boot_id: crate::daemon::read_boot_id().unwrap(),
                host_incarnation: "binding-test".into(),
            }),
            launch: chat::Launch {
                provider: Provider::Omp,
                session_id: None,
                recipe: crate::model::LaunchRecipe {
                    program: "/bin/cat".into(),
                    arguments: vec![],
                    working_directory: path.into(),
                    environment: BTreeMap::new(),
                    unset_environment: Default::default(),
                },
            },
            terminal: None,
            native_continuity: Default::default(),
            owned_codex_listener: None,
        }
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
        assert!(
            fixture
                .return_refuses(a.clone())
                .contains("captured native backend changed")
        );
        assert!(
            !second.0.is_completed(),
            "stale A consent cannot close real B"
        );
        second.0.close().unwrap();
        fixture.retire();
        assert!(
            !fixture.validates(&a),
            "B's retirement never recreates A authority"
        );
        assert!(
            fixture.validates(&binding_b),
            "exact latest history survives retirement"
        );
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
        assert!(
            fixture
                .return_refuses(a.clone())
                .contains("captured native backend changed")
        );
        assert!(!owner.0.is_completed());
        let resolved = fixture.resolve();
        assert_ne!(
            resolved.backend, a.backend,
            "even a proved failed restart cannot revive an old user intent"
        );
        let mut state = fixture.host.lock().unwrap();
        for _ in 0..CAPACITY {
            state
                .native_sources
                .installed(TerminalId::new(), uuid::Uuid::new_v4());
        }
        assert!(!state.native_sources.validates(&resolved));
        assert!(state.native_sources.entries.len() <= CAPACITY);
    }
    #[test]
    fn publication_binds_full_source_and_exact_paired_worker_even_on_the_same_backend() {
        let directory = tempfile::tempdir_in("/var/tmp").unwrap();
        let source = source(directory.path());
        let owner = actor(&source);
        let fixture = BackendFixture::new(source.clone(), owner.0.clone(), uuid::Uuid::new_v4());
        let before = fixture.capture().unwrap();
        let mut state = fixture.host.lock().unwrap();
        let witness = state.native_sources.entries[&source.id]
            .known
            .as_ref()
            .unwrap()
            .0
            .clone();
        let terminal = crate::terminal_ipc::TerminalDescriptor::relaunchable(
            source.id,
            source.launch.recipe.clone(),
            None,
        )
        .session;
        let mut paired_source = source.clone();
        paired_source.terminal = Some(terminal);
        let first_worker = uuid::Uuid::new_v4();
        let paired = state
            .native_sources
            .publish(
                paired_source.clone(),
                before.execution_instance,
                witness.clone(),
                Some(first_worker),
                false,
            )
            .unwrap();
        assert_ne!(
            paired.backend, before.backend,
            "pair publication cannot reuse no-pair consent"
        );
        assert!(!state.native_sources.validates(&before));
        let successor = state
            .native_sources
            .publish(
                paired_source.clone(),
                before.execution_instance,
                witness.clone(),
                Some(uuid::Uuid::new_v4()),
                false,
            )
            .unwrap();
        assert_ne!(
            successor.backend, paired.backend,
            "same descriptor cannot alias another concrete terminal worker"
        );
        assert!(!state.native_sources.validates(&paired));
        let restored = state
            .native_sources
            .publish(
                source.clone(),
                before.execution_instance,
                witness,
                None,
                false,
            )
            .unwrap();
        assert_ne!(
            restored.backend, before.backend,
            "retired pair never revives earlier source consent"
        );
        assert!(!state.native_sources.authorizes_intent(&before));
    }

    struct Learns;
    impl Adapter for Learns {
        fn initialize(&mut self) -> Result<Step, String> {
            Ok(Step::default())
        }
        fn receive(&mut self, value: serde_json::Value) -> Result<Step, String> {
            let threads: Vec<&str> = if value["thread"] == "batch" {
                vec!["first", "second"]
            } else if value["thread"] == "aba" {
                vec!["second", "first"]
            } else {
                vec![value["thread"].as_str().unwrap()]
            };
            Ok(Step {
                events: threads
                    .into_iter()
                    .map(|id| chat::Event::Session {
                        id: id.into(),
                        title: None,
                    })
                    .collect(),
                ..Step::default()
            })
        }
        fn command(&mut self, command: chat::Command) -> Result<Step, String> {
            let chat::Command::SetModel { model, .. } = command else {
                return Ok(Step::default());
            };
            Ok(Step {
                writes: vec![serde_json::json!({"thread": model})],
                ..Step::default()
            })
        }
    }

    #[test]
    fn first_confirmed_thread_learning_keeps_only_the_original_unknown_intent() {
        let directory = tempfile::tempdir_in("/var/tmp").unwrap();
        let source = source(directory.path());
        let owner = actor_with(&source, Box::new(Learns));
        let fixture = BackendFixture::new(source.clone(), owner.0.clone(), uuid::Uuid::new_v4());
        let unknown = fixture.capture().unwrap();
        let learn = |thread: &str| {
            owner
                .0
                .command(chat::Command::SetModel {
                    model: thread.into(),
                    reasoning: None,
                })
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(2);
            while owner.0.confirmed_session_id().as_deref() != Some(thread) {
                assert!(
                    Instant::now() < deadline,
                    "actual provider pipe must confirm {thread}"
                );
                thread::sleep(Duration::from_millis(2));
            }
        };
        learn("first");
        let first = fixture.capture().unwrap(); // UI may read before Return.
        assert_eq!(first.source.launch.session_id.as_deref(), Some("first"));
        assert_eq!(require(&fixture.host, 1, &unknown).unwrap(), first);
        assert!(
            !fixture.validates(&unknown),
            "old unknown is not general receipt authority"
        );
        learn("second");
        let second = fixture.capture().unwrap();
        assert_eq!(second.source.launch.session_id.as_deref(), Some("second"));
        assert!(require(&fixture.host, 1, &unknown).is_err());
        assert!(require(&fixture.host, 1, &first).is_err());
        assert!(
            !owner.0.is_completed(),
            "known-thread or old unknown refusal has no teardown effects"
        );
    }

    #[test]
    fn typed_handoff_ack_authenticates_the_full_operation_and_never_clears_a_successor() {
        let directory = tempfile::tempdir_in("/var/tmp").unwrap();
        let source = source(directory.path());
        let owner = actor(&source);
        let fixture = BackendFixture::new(source.clone(), owner.0.clone(), uuid::Uuid::new_v4());
        let binding = fixture.capture().unwrap();
        // Metadata-only issuer fixture: it deliberately grants NO terminal
        // teardown or source-transition authority; ACK must not require either.
        let mut descriptor = crate::terminal_ipc::TerminalDescriptor::relaunchable(
            TerminalId::new(),
            source.launch.recipe.clone(),
            None,
        );
        descriptor.execution_instance = uuid::Uuid::new_v4();
        let a = TerminalHandoffReceipt {
            operation: uuid::Uuid::new_v4(),
            source: source.clone(),
            source_execution_instance: binding.execution_instance,
            source_binding: binding,
            retained_binding: None,
            terminal_native_epoch: uuid::Uuid::new_v4(),
            terminal: descriptor,
        };
        let mut b = a.clone();
        b.operation = uuid::Uuid::new_v4();
        b.terminal.execution_instance = uuid::Uuid::new_v4();
        {
            let mut state = fixture.host.lock().unwrap();
            state.remember_terminal_handoff(a.clone());
            state.remember_terminal_handoff(b.clone());
            state.chat_handoffs.insert(
                source.id,
                TerminalHandoff::from_receipt(
                    b.clone(),
                    0,
                    BTreeMap::new(),
                    Instant::now() + ACKNOWLEDGEMENT_GRACE,
                ),
            );
        }
        let mut forged = b.clone();
        forged.terminal.execution_instance = a.terminal.execution_instance;
        assert!(
            process_inner(
                &fixture.host,
                1,
                ClientRequest::ChatCommitTerminalReceipt { receipt: forged },
                None
            )
            .is_err()
        );
        assert!(
            process_inner(
                &fixture.host,
                1,
                ClientRequest::ChatCommitTerminal {
                    terminal_id: a.terminal.session.id
                },
                None
            )
            .is_err()
        );
        process_inner(
            &fixture.host,
            1,
            ClientRequest::ChatCommitTerminalReceipt { receipt: a },
            None,
        )
        .unwrap();
        assert_eq!(
            fixture.host.lock().unwrap().chat_handoffs[&source.id]
                .issued
                .as_ref(),
            Some(&b)
        );
        for _ in 0..2 {
            process_inner(
                &fixture.host,
                1,
                ClientRequest::ChatCommitTerminalReceipt { receipt: b.clone() },
                None,
            )
            .unwrap();
            assert!(
                !fixture
                    .host
                    .lock()
                    .unwrap()
                    .chat_handoffs
                    .contains_key(&source.id)
            );
        }
        assert!(
            !owner.0.is_completed(),
            "metadata ACK never stops any provider or terminal"
        );
    }

    #[test]
    fn unobserved_conversation_transitions_never_promote_old_unknown_return_consent() {
        for one_frame in [false, true] {
            let directory = tempfile::tempdir_in("/var/tmp").unwrap();
            let source = source(directory.path());
            let owner = actor_with(&source, Box::new(Learns));
            let fixture = BackendFixture::new(source, owner.0.clone(), uuid::Uuid::new_v4());
            let unknown = fixture.capture().unwrap();
            let send = |value: &str, confirmed: &str| {
                owner
                    .0
                    .command(chat::Command::SetModel {
                        model: value.into(),
                        reasoning: None,
                    })
                    .unwrap();
                let deadline = Instant::now() + Duration::from_secs(2);
                while owner.0.confirmed_session_id().as_deref() != Some(confirmed) {
                    assert!(
                        Instant::now() < deadline,
                        "actual provider frame did not confirm {confirmed}"
                    );
                    thread::sleep(Duration::from_millis(2));
                }
            };
            if one_frame {
                send("batch", "second");
            } else {
                send("first", "first");
                send("second", "second");
            }
            // No capture, ChatRead or host observation happened between first
            // and second. The actual producer history must still fence Return.
            assert!(require(&fixture.host, 1, &unknown).is_err());
            assert!(
                fixture
                    .return_refuses(unknown)
                    .contains("captured native backend changed")
            );
            assert!(!owner.0.is_completed());
        }
    }

    #[test]
    fn returned_resource_native_publication_or_ambiguous_start_survives_retirement() {
        let directory = tempfile::tempdir_in("/var/tmp").unwrap();
        let source = source(directory.path());
        let original = actor(&source);
        let fixture = BackendFixture::new(source.clone(), original.0.clone(), uuid::Uuid::new_v4());
        let binding = fixture.capture().unwrap();
        // A returned worker may already have retired: only its positive,
        // bounded publication history remains. No absence grants authority.
        let mut terminal = crate::terminal_ipc::TerminalDescriptor::relaunchable(
            TerminalId::new(),
            source.launch.recipe.clone(),
            None,
        );
        terminal.execution_instance = uuid::Uuid::new_v4();
        let receipt = {
            let mut state = fixture.host.lock().unwrap();
            state
                .published_terminal_workers
                .observe(terminal.session.id, terminal.execution_instance);
            let receipt = TerminalHandoffReceipt {
                operation: uuid::Uuid::new_v4(),
                source: source.clone(),
                source_execution_instance: binding.execution_instance,
                source_binding: binding.clone(),
                retained_binding: None,
                terminal_native_epoch: state
                    .native_sources
                    .retain_resource_epoch(terminal.session.id),
                terminal: terminal.clone(),
            };
            state.remember_terminal_handoff(receipt.clone());
            state.validate_terminal_handoff_receipt(&receipt).unwrap();
            receipt
        };
        let mut native_source = source.clone();
        native_source.id = terminal.session.id;
        let successor = actor(&native_source); // actual independent provider process
        {
            let mut state = fixture.host.lock().unwrap();
            let instance = uuid::Uuid::new_v4();
            state.native_sources.installed(native_source.id, instance);
            state.chats.insert(
                native_source.id,
                Record {
                    execution_instance: instance,
                    console_recipe: None,
                    paired_terminal: None,
                    handoff_receipt: None,
                    source_binding: None,
                    session: native_source.clone(),
                    handle: successor.0.clone(),
                    source_process: None,
                    subscribers: BTreeMap::from([(1, false)]),
                    observations: BTreeMap::new(),
                    completion_observed: false,
                    returning_terminal: false,
                },
            );
            assert!(state.validate_terminal_handoff_receipt(&receipt).is_err());
        }
        successor.0.close().unwrap();
        {
            let mut state = fixture.host.lock().unwrap();
            drop(state.chats.remove(&native_source.id));
            assert!(state.chats.get(&native_source.id).is_none());
            assert_eq!(
                state.published_terminal_workers.latest(native_source.id),
                Some(terminal.execution_instance)
            );
            assert!(
                state.native_sources.validates(&binding),
                "original C0 backend history is unchanged"
            );
            assert!(
                state.validate_terminal_handoff_receipt(&receipt).is_err(),
                "retired B cannot restore C0 -> T1 consent"
            );
        }
        // A DIFFERENT returned resource is fenced before any provider creation,
        // including failure before Record install. Clearing pending metadata
        // cannot repair that explicitly unresolved transition.
        let pending_id = TerminalId::new();
        let mut pending = receipt.clone();
        pending.operation = uuid::Uuid::new_v4();
        pending.terminal.session.id = pending_id;
        {
            let mut state = fixture.host.lock().unwrap();
            state
                .published_terminal_workers
                .observe(pending_id, pending.terminal.execution_instance);
            pending.terminal_native_epoch = state.native_sources.retain_resource_epoch(pending_id);
            state.remember_terminal_handoff(pending.clone());
            state.validate_terminal_handoff_receipt(&pending).unwrap();
            state.native_sources.mutating(pending_id); // actual production pre-creation seam
            assert!(!state.chats.contains_key(&pending_id));
            assert!(state.validate_terminal_handoff_receipt(&pending).is_err());
            for _ in 0..CAPACITY {
                state
                    .native_sources
                    .retain_resource_epoch(TerminalId::new());
            }
            assert_eq!(state.native_sources.resource_epoch(pending_id), None);
            assert!(
                state.validate_terminal_handoff_receipt(&pending).is_err(),
                "eviction never reconstructs an absence stamp"
            );
        }
        assert!(!original.0.is_completed());
    }

    #[test]
    fn same_final_conversation_aba_revokes_issued_receipt_without_any_read_refresh() {
        let directory = tempfile::tempdir_in("/var/tmp").unwrap();
        let source = source(directory.path());
        let instance = uuid::Uuid::new_v4();
        let callback_host = Arc::new(Mutex::new(Weak::<HostLock>::new()));
        let target = callback_host.clone();
        let id = source.id;
        let owner = actor_with_notify(&source, Box::new(Learns), move |update| {
            let weak = target.lock().unwrap().clone();
            changed_owned(&weak, id, instance, update);
        });
        let fixture = BackendFixture::new(source.clone(), owner.0.clone(), instance);
        *callback_host.lock().unwrap() = Arc::downgrade(&fixture.host);
        owner
            .0
            .command(chat::Command::SetModel {
                model: "first".into(),
                reasoning: None,
            })
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while owner.0.confirmed_session_id().as_deref() != Some("first") {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(2));
        }
        let binding = fixture.capture().unwrap();
        let revision = owner.0.observed_backend_revision().unwrap();
        let receipt = {
            let mut state = fixture.host.lock().unwrap();
            let mut terminal = crate::terminal_ipc::TerminalDescriptor::relaunchable(
                TerminalId::new(),
                source.launch.recipe.clone(),
                None,
            );
            terminal.execution_instance = uuid::Uuid::new_v4();
            state
                .published_terminal_workers
                .observe(terminal.session.id, terminal.execution_instance);
            let receipt = TerminalHandoffReceipt {
                operation: uuid::Uuid::new_v4(),
                source: binding.source.clone(),
                source_execution_instance: instance,
                source_binding: binding.clone(),
                retained_binding: None,
                terminal_native_epoch: state
                    .native_sources
                    .retain_resource_epoch(terminal.session.id),
                terminal,
            };
            state.remember_terminal_handoff(receipt.clone());
            state.validate_terminal_handoff_receipt(&receipt).unwrap();
            receipt
        };
        owner
            .0
            .command(chat::Command::SetModel {
                model: "aba".into(),
                reasoning: None,
            })
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while fixture.validates(&binding) {
            assert!(
                Instant::now() < deadline,
                "actual identity callback must revoke same-final-thread authority without a fresh Read"
            );
            thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(owner.0.confirmed_session_id().as_deref(), Some("first"));
        assert_eq!(owner.0.observed_backend_revision(), Some(revision + 2));
        assert!(
            fixture
                .host
                .lock()
                .unwrap()
                .validate_terminal_handoff_receipt(&receipt)
                .is_err()
        );
        assert!(!owner.0.is_completed());
    }

    #[test]
    fn oversized_handoff_metadata_refuses_before_any_owned_provider_or_terminal_effect() {
        let directory = tempfile::tempdir_in("/var/tmp").unwrap();
        let mut source = source(directory.path());
        source
            .launch
            .recipe
            .environment
            .insert("HANDOFF_METADATA_FIXTURE".into(), "x".repeat(70 * 1024));
        let owner = actor(&source);
        let fixture = BackendFixture::new(source.clone(), owner.0.clone(), uuid::Uuid::new_v4());
        let binding = fixture.capture().unwrap();
        let refusal = fixture.return_refuses(binding);
        assert!(refusal.contains("acknowledgement budget"), "{refusal}");
        assert!(
            !owner.0.is_completed(),
            "preflight must precede provider teardown"
        );
        let state = fixture.host.lock().unwrap();
        assert!(!state.chats[&source.id].returning_terminal);
        assert!(state.terminals.is_empty());
        assert!(state.chat_handoffs.is_empty());
    }
}
