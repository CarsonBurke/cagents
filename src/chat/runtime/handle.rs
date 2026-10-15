//! A daemon handle refers either to its local actor or the exact actor retained
//! by a native helper. Remote handles never instantiate another reducer/store.
use super::remote_actor::{ActorIdentity, InputIdentity, InputReceipt, OutboxEntry, RemoteActor};
use super::{LocalHandle, Rejection, Store, Update, UpdateReadiness};
use crate::chat::{
    self, Command, Snapshot,
    protocol::{Adapter, ProcessSpec, Step},
};
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct BackendIdentity {
    helper: Option<uuid::Uuid>,
    actor: uuid::Uuid,
    process: crate::daemon::holder_registry::Process,
    conversation: Option<String>,
    conversation_revision: u64,
}
impl BackendIdentity {
    pub(super) fn matches_local(&self, local: &LocalHandle) -> bool {
        let (thread, revision) = local.backend_confirmation();
        local.backend_is_observed()
            && self.actor == local.backend_instance()
            && self.conversation == thread
            && self.conversation_revision == revision
            && local.confirmed_session_id() == thread
            && self.valid()
    }
    pub(super) fn matches_actor(&self, actor: &super::Actor) -> bool {
        let current = actor
            .backend_conversation
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let process = actor.native_peer.clone().or_else(|| {
            actor
                .provider_process
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone()
        });
        !actor
            .close_state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .unobserved_native
            && self.actor == actor.backend_instance
            && process.as_ref() == Some(&self.process)
            && self.conversation == current.thread
            && self.conversation_revision == current.revision
            && *actor
                .confirmed_session
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                == current.thread
            && self.valid()
    }
    pub(super) fn belongs_to_helper(&self, helper: uuid::Uuid) -> bool {
        self.helper == Some(helper)
    }
    pub(crate) fn conversation(&self) -> Option<String> {
        self.conversation.clone()
    }
    pub(crate) fn same_backend(&self, other: &Self) -> bool {
        self.helper == other.helper && self.actor == other.actor && self.process == other.process
    }
    pub(crate) fn conversation_revision(&self) -> u64 {
        self.conversation_revision
    }
    pub(crate) fn first_confirmation(&self, other: &Self) -> bool {
        self.same_backend(other)
            && self.conversation.is_none()
            && other.conversation.is_some()
            && self.conversation_revision.checked_add(1) == Some(other.conversation_revision)
    }
    pub(crate) fn valid(&self) -> bool {
        !self.actor.is_nil()
            && self.process.pid > 0
            && self.process.start > 0
            && self.conversation_revision != u64::MAX
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) enum BackendSourceClaim {
    Granted(uuid::Uuid),
    Refused(String),
}
impl BackendSourceClaim {
    #[cfg(test)]
    pub(crate) fn into_result(self) -> Result<uuid::Uuid, String> {
        match self {
            Self::Granted(token) => Ok(token),
            Self::Refused(reason) => Err(reason),
        }
    }
}

#[derive(Clone)]
pub struct Handle(Owner);
#[derive(Clone)]
enum Owner {
    Local(LocalHandle),
    Remote(RemoteActor),
}
impl Handle {
    /// Exact authenticated helper metadata. Callers must compare the complete
    /// record against an installed host entry; this is never cleanup authority.
    pub(crate) fn native_holder_record(&self) -> Option<&crate::daemon::holder_registry::Record> {
        match &self.0 {
            Owner::Local(local) => local.native_holder_record(),
            Owner::Remote(remote) => Some(remote.native_holder_record()),
        }
    }
    pub(crate) fn try_close_identity(&self) -> Result<super::CloseIdentity, String> {
        match &self.0 {
            Owner::Local(local) => local.try_close_identity(),
            Owner::Remote(remote) => remote.try_close_identity(),
        }
    }
    #[cfg(test)]
    pub(crate) fn with_presentation_state_locked(&self, action: impl FnOnce()) {
        self.local_actor()
            .expect("fixture requires the actual Local owner")
            .with_presentation_state_locked(action);
    }

    #[cfg(test)]
    pub(crate) fn with_presentation_store_locked(&self, action: impl FnOnce()) {
        self.local_actor()
            .expect("fixture requires the actual Local owner")
            .with_presentation_store_locked(action);
    }

    #[cfg(test)]
    pub(super) fn from_remote_fixture(remote: RemoteActor) -> Self {
        Self(Owner::Remote(remote))
    }
    #[cfg(test)]
    pub(crate) fn start(
        spec: ProcessSpec,
        adapter: Box<dyn Adapter>,
        initial: Step,
        store: Store,
        session_id: Option<String>,
        notify: impl Fn(Update) + Send + Sync + 'static,
    ) -> Result<Self, String> {
        LocalHandle::start(spec, adapter, initial, store, session_id, notify)
            .map(|local| Self(Owner::Local(local)))
    }
    #[cfg(test)]
    pub(crate) fn fixture_retained_native_holder(
        &self,
        holder: super::super::native_holder::Holder,
        reason: String,
    ) {
        self.local_actor()
            .expect("the host shutdown fixture requires a local actor")
            .fixture_retained_native_holder(holder, reason);
    }

    pub(crate) fn start_owned(
        spec: ProcessSpec,
        adapter: Box<dyn Adapter>,
        initial: Step,
        store: Store,
        source: chat::Session,
        recovery_environment: BTreeMap<String, String>,
        notify: impl Fn(Update) + Send + Sync + 'static,
    ) -> Result<Self, String> {
        LocalHandle::start_owned(
            spec,
            adapter,
            initial,
            store,
            source,
            recovery_environment,
            None,
            notify,
        )
        .map(|local| Self(Owner::Local(local)))
    }
    pub(crate) fn start_adopted(
        spec: ProcessSpec,
        adapter: Box<dyn Adapter>,
        initial: Step,
        store: Store,
        recovery: chat::native_holder::Recovery,
        holder: chat::native_holder::Holder,
        notify: impl Fn(Update) + Send + Sync + 'static,
    ) -> Result<Self, String> {
        LocalHandle::start_adopted(
            spec, adapter, initial, store, recovery, holder, None, notify,
        )
        .map(|local| Self(Owner::Local(local)))
    }
    pub(super) fn local_actor(&self) -> Result<&LocalHandle, String> {
        match &self.0 {Owner::Local(local)=>Ok(local),Owner::Remote(_)=>Err("A retained actor must reconnect to its existing reducer; never replace it with a local provider".into())}
    }
    pub(crate) fn start_owned_with_previous(
        spec: ProcessSpec,
        adapter: Box<dyn Adapter>,
        initial: Step,
        store: Store,
        source: chat::Session,
        recovery_environment: BTreeMap<String, String>,
        previous: Option<&Handle>,
        notify: impl Fn(Update) + Send + Sync + 'static,
    ) -> Result<Self, String> {
        let previous = previous.map(Handle::local_actor).transpose()?;
        LocalHandle::start_owned(
            spec,
            adapter,
            initial,
            store,
            source,
            recovery_environment,
            previous,
            notify,
        )
        .map(|local| Self(Owner::Local(local)))
    }
    pub(crate) fn start_adopted_with_previous(
        spec: ProcessSpec,
        adapter: Box<dyn Adapter>,
        initial: Step,
        store: Store,
        recovery: chat::native_holder::Recovery,
        holder: chat::native_holder::Holder,
        previous: Option<&Handle>,
        notify: impl Fn(Update) + Send + Sync + 'static,
    ) -> Result<Self, String> {
        let previous = previous.map(Handle::local_actor).transpose()?;
        LocalHandle::start_adopted(
            spec, adapter, initial, store, recovery, holder, previous, notify,
        )
        .map(|local| Self(Owner::Local(local)))
    }
    pub(crate) fn restart_remote(
        &self,
        id: uuid::Uuid,
        spec: ProcessSpec,
        source: chat::Session,
        recovery_environment: BTreeMap<String, String>,
    ) -> Result<super::remote_actor::RestartReceipt, String> {
        match &self.0 {
            Owner::Remote(remote) => remote.restart(id, spec, source, recovery_environment),
            Owner::Local(_) => {
                Err("This actor is local; use its existing local restart owner".into())
            }
        }
    }
    pub(crate) fn restart_environment(&self) -> crate::terminal_ipc::ChatExecutionEnvironment {
        match &self.0 {
            Owner::Local(local) => local.restart_environment(),
            Owner::Remote(remote) => {
                crate::terminal_ipc::ChatExecutionEnvironment(remote.restart_environment())
            }
        }
    }
    /// The host selects this after preparing the exact authorized launch. It
    /// must drain/commit/ACK the outbox; ordinary notification is only a wakeup.
    pub(crate) fn start_remote_owned(
        spec: ProcessSpec,
        source: chat::Session,
        recovery_environment: BTreeMap<String, String>,
        notify: impl Fn(Update) + Send + Sync + 'static,
    ) -> Result<Self, String> {
        RemoteActor::spawn(spec, source, recovery_environment, notify)
            .map(|remote| Self(Owner::Remote(remote)))
    }
    pub(crate) fn adopt_remote(
        record: &crate::daemon::holder_registry::Record,
        source: chat::Session,
        proof: crate::model::HostSessionProof,
        notify: impl Fn(Update) + Send + Sync + 'static,
    ) -> Result<Self, String> {
        RemoteActor::adopt(record, source, proof, notify).map(|remote| Self(Owner::Remote(remote)))
    }
    pub(crate) fn inspect_remote(
        record: &crate::daemon::holder_registry::Record,
        proof: crate::model::HostSessionProof,
    ) -> Result<super::remote_actor::ActorInspection, String> {
        RemoteActor::inspect(record, proof)
    }
    pub(crate) fn is_remote_record(record: &crate::daemon::holder_registry::Record) -> bool {
        RemoteActor::is_record(record)
    }
    /// Read-only proof for an exact cold cleanup retry. This proves only the
    /// named actor's semantic/helper retirement, never absence of a replacement
    /// or permission to delete account/session history.
    pub(crate) fn retained_retirement_proved(
        record: &crate::daemon::holder_registry::Record,
        proof: &crate::model::HostSessionProof,
    ) -> Result<bool, String> {
        RemoteActor::retirement_proved(record, proof)
    }
    pub(crate) fn wake_retained_outcomes(&self) {
        if let Owner::Remote(remote) = &self.0 {
            remote.wake_outcomes();
        }
    }
    pub(crate) fn uncommitted_outcomes(&self) -> Result<Vec<OutboxEntry>, String> {
        match &self.0 {
            Owner::Remote(remote) => {
                remote.refresh_state()?;
                remote.call("outbox_unacked", serde_json::Value::Null)
            }
            _ => Err("This actor has no retained outcome outbox".into()),
        }
    }
    /// Current provider incarnation; the immutable retained helper Record.root
    /// intentionally continues identifying the original incarnation.
    /// Remote callers needing a destructive fence must refresh outside HostLock
    /// while holding their lifecycle gate before reading this cached metadata.
    pub(crate) fn provider_process(&self) -> Option<crate::daemon::holder_registry::Process> {
        match &self.0 {
            Owner::Local(local) => local.provider_process(),
            Owner::Remote(remote) => remote.state().provider_process.clone(),
        }
    }
    /// Fresh concrete backend evidence. This is an authenticated helper RPC for
    /// retained actors, never the cached Record.root or presentation snapshot.
    /// Call off HostLock while retaining the per-chat lifecycle transition.
    pub(crate) fn backend_identity(
        &self,
        proof: &crate::model::HostSessionProof,
    ) -> Result<BackendIdentity, String> {
        match &self.0 {
            Owner::Remote(remote) => {
                // An impossible caller generation forces a fresh response (or
                // conservative refusal on the exact wrap value). Unlike inspect,
                // state is guarded by the helper's current attachment epoch.
                let state: Option<super::remote_actor::Resident> =
                    remote.call("state", serde_json::json!([u64::MAX]))?;
                let state = state.ok_or("Fresh retained backend evidence is unavailable")?;
                if state.backend_confirmation_pending {
                    return Err("Retained provider confirmation is still transitioning".into());
                }
                Ok(BackendIdentity {
                    helper: Some(remote.identity().instance),
                    actor: state
                        .backend_instance
                        .ok_or("Retained helper has no concrete backend incarnation evidence")?,
                    conversation: state.confirmed_session,
                    conversation_revision: state
                        .backend_conversation_revision
                        .ok_or("Retained helper lacks exact conversation transition history")?,
                    process: state
                        .provider_process
                        .ok_or("Retained actor has no authenticated concrete backend identity")?,
                })
            }
            Owner::Local(local) => {
                if !local.backend_is_observed() {
                    return Err("The retained native backend is not observed by this actor; reattach before confirming a handoff".into());
                }
                let process = if let Some(process) = local.provider_process() {
                    process
                } else {
                    let record = local
                        .native_holder_record()
                        .ok_or("Native actor has no concrete backend identity")?;
                    let (recovery, _) = chat::native_holder::inspect(record, proof)?;
                    recovery
                        .server_process
                        .ok_or("Native holder has no authenticated concrete server identity")?
                };
                let (conversation, conversation_revision) = local.backend_confirmation();
                if conversation != local.confirmed_session_id() {
                    return Err("Provider identity publication is still transitioning".into());
                }
                Ok(BackendIdentity {
                    helper: None,
                    actor: local.backend_instance(),
                    process,
                    conversation,
                    conversation_revision,
                })
            }
        }
    }
    /// Cheap resident metadata for producer callbacks, never a helper RPC.
    pub(crate) fn observed_backend_revision(&self) -> Option<u64> {
        match &self.0 {
            Owner::Local(local) => Some(local.backend_confirmation().1),
            Owner::Remote(remote) => remote.state().backend_conversation_revision,
        }
    }
    /// Cheap coherence recheck after an off-host authenticated state read.
    /// A producer revocation that raced that read cannot be republished later.
    pub(crate) fn backend_identity_is_current(&self, expected: &BackendIdentity) -> bool {
        match &self.0 {
            Owner::Local(local) => {
                let (thread, revision) = local.backend_confirmation();
                local.backend_is_observed()
                    && local.backend_instance() == expected.actor
                    && thread == expected.conversation
                    && revision == expected.conversation_revision
                    && local.confirmed_session_id() == thread
            }
            Owner::Remote(remote) => {
                let current = remote.state();
                expected.helper == Some(remote.identity().instance)
                    && current.backend_instance == Some(expected.actor)
                    && current.backend_conversation_revision == Some(expected.conversation_revision)
                    && !current.backend_confirmation_pending
                    && current.confirmed_session == expected.conversation
                    && current.provider_process.as_ref() == Some(&expected.process)
            }
        }
    }
    pub(crate) fn claim_backend_source(
        &self,
        expected: &BackendIdentity,
    ) -> Result<BackendSourceClaim, String> {
        match &self.0 {
            Owner::Local(local) if expected.helper.is_none() => {
                local.claim_backend_source(expected)
            }
            Owner::Remote(remote) if expected.helper == Some(remote.identity().instance) => {
                remote.call("claim_backend_source", serde_json::json!([expected]))
            }
            _ => Err("The actual backend owner changed before handoff admission".into()),
        }
    }
    #[cfg(test)]
    pub(crate) fn claim_backend_source_losing_reply_for_test(
        &self,
        expected: &BackendIdentity,
    ) -> Result<(), String> {
        self.local_actor()?
            .claim_backend_source_losing_reply_for_test(expected)
    }
    #[cfg(test)]
    pub(crate) fn withdraw_backend_observation_for_test(&self) {
        self.local_actor()
            .expect("fixture requires actual Local actor")
            .withdraw_backend_observation_for_test();
    }
    #[cfg(test)]
    pub(crate) fn lose_next_source_release_reply_for_test(&self) {
        self.local_actor()
            .expect("fixture requires actual Local actor")
            .lose_next_source_release_reply_for_test();
    }
    pub(crate) fn release_backend_source(
        &self,
        expected: &BackendIdentity,
        claim: uuid::Uuid,
    ) -> Result<(), String> {
        match &self.0 {
            Owner::Local(local) if expected.helper.is_none() => {
                local.release_backend_source(expected, claim)
            }
            Owner::Remote(remote) if expected.helper == Some(remote.identity().instance) => remote
                .call(
                    "release_backend_source",
                    serde_json::json!([expected, claim]),
                ),
            _ => Err("The actual backend owner changed before handoff disposition".into()),
        }
    }
    pub(crate) fn close_backend_source(
        &self,
        expected: &BackendIdentity,
        claim: uuid::Uuid,
    ) -> Result<(), String> {
        match &self.0 {
            Owner::Local(local) if expected.helper.is_none() => {
                local.close_backend_source(expected, claim)
            }
            Owner::Remote(remote) if expected.helper == Some(remote.identity().instance) => {
                remote.close_method("close_backend_source", serde_json::json!([expected, claim]))
            }
            _ => Err("The actual backend owner changed; no provider was stopped".into()),
        }
    }
    pub(crate) fn update_claimed_native_source(
        &self,
        expected: &BackendIdentity,
        claim: uuid::Uuid,
        source: chat::Session,
        console: Option<crate::model::LaunchRecipe>,
        pair: Option<crate::terminal::holder::Record>,
    ) -> Result<(), String> {
        match &self.0 {
            Owner::Local(local) if expected.helper.is_none() => {
                local.update_claimed_native_source(expected, claim, source, console, pair)
            }
            Owner::Remote(remote) if expected.helper == Some(remote.identity().instance) => remote
                .call(
                    "update_claimed_source",
                    serde_json::json!([expected, claim, source, console, pair]),
                ),
            _ => Err("The actual backend owner changed; no source was updated".into()),
        }
    }
    pub(crate) fn actor_identity(&self) -> Option<ActorIdentity> {
        match &self.0 {
            Owner::Remote(remote) => Some(remote.identity()),
            _ => None,
        }
    }
    pub(crate) fn retained_outcomes(&self, after: u64) -> Result<Vec<OutboxEntry>, String> {
        match &self.0 {
            Owner::Remote(remote) => remote.call("outbox", serde_json::json!([after])),
            _ => Err("This actor has no remote outbox".into()),
        }
    }
    pub(crate) fn commit_retained_outcomes(&self, through: u64) -> Result<(), String> {
        match &self.0 {
            Owner::Remote(remote) => remote.call("ack_outbox", serde_json::json!([through])),
            _ => Err("This actor has no remote outbox".into()),
        }
    }
    pub(crate) fn retained_input(
        &self,
        identity: InputIdentity,
    ) -> Result<Option<InputReceipt>, String> {
        match &self.0 {
            Owner::Remote(remote) => remote.call("input_receipt", serde_json::json!([identity])),
            _ => Err("This actor has no remote input journal".into()),
        }
    }
    pub(crate) fn pending_retained_inputs(&self) -> Vec<InputIdentity> {
        match &self.0 {
            Owner::Remote(remote) => remote.pending_inputs(),
            _ => Vec::new(),
        }
    }
    pub(crate) fn retry_retained_input(
        &self,
        identity: &InputIdentity,
    ) -> Result<InputReceipt, String> {
        match &self.0 {
            Owner::Remote(remote) => remote.retry_input(identity),
            _ => Err("This actor has no retained input envelope".into()),
        }
    }
    pub(crate) fn commit_retained_inputs(&self, through: u64) -> Result<(), String> {
        match &self.0 {
            Owner::Remote(remote) => remote.call("ack_inputs", serde_json::json!([through])),
            _ => Err("This actor has no remote input journal".into()),
        }
    }
    pub(crate) fn same_actor(&self, other: &Self) -> bool {
        match (&self.0, &other.0) {
            (Owner::Local(a), Owner::Local(b)) => a.same_actor(b),
            (Owner::Remote(a), Owner::Remote(b)) => a.identity() == b.identity(),
            _ => false,
        }
    }
    pub(crate) fn enqueue_command(
        &self,
        command: Command,
    ) -> Result<crossbeam_channel::Receiver<Result<(), String>>, String> {
        match &self.0 {
            Owner::Local(local) => local.enqueue_command(command),
            Owner::Remote(remote) => remote.enqueue(None, command),
        }
    }
    /// This is only a compare-and-enqueue primitive, never host permission.
    /// The caller retains its original authenticated witness; no getter,
    /// restart, source learning or process selection occurs here.
    pub(crate) fn enqueue_conditional_interrupt(
        &self,
        expected: BackendIdentity,
    ) -> Result<crossbeam_channel::Receiver<Result<(), String>>, String> {
        match &self.0 {
            Owner::Local(local) if expected.helper.is_none() && expected.valid() => {
                local.enqueue_actor(super::ActorCommand::ConditionalInterrupt { expected })
            }
            Owner::Remote(remote) => remote.enqueue_conditional_interrupt(expected),
            _ => Err("The captured backend is not owned by this actor".into()),
        }
    }
    pub fn command(&self, command: Command) -> Result<(), String> {
        self.try_command(command).map_err(String::from)
    }
    pub(crate) fn try_command(&self, command: Command) -> Result<(), Rejection> {
        match &self.0 {
            Owner::Local(local) => local.try_command(command),
            Owner::Remote(remote) => remote.submit(None, command),
        }
    }
    pub(crate) fn command_for_run(
        &self,
        run: crate::model::RunId,
        command: Command,
    ) -> Result<(), String> {
        match &self.0 {
            Owner::Local(local) => local.command_for_run(run, command),
            Owner::Remote(remote) => remote.submit(Some(run), command).map_err(String::from),
        }
    }
    pub(crate) fn pinned_execution_directory(
        &self,
        candidate: &std::path::Path,
    ) -> Result<std::sync::Arc<super::super::protocol::ExecutionContext>, String> {
        match &self.0 {
            Owner::Local(local) => local.pinned_execution_directory(candidate),
            Owner::Remote(remote) => remote.pinned_directory(candidate),
        }
    }
    pub(crate) fn capture_execution_directory(
        &self,
        candidate: &std::path::Path,
    ) -> Result<super::DirectoryProof, String> {
        match &self.0 {
            Owner::Local(local) => local.capture_execution_directory(candidate),
            Owner::Remote(remote) => remote.capture_directory(candidate),
        }
    }
    pub(crate) fn relocate_execution_directory(
        &self,
        candidate: &std::path::Path,
    ) -> Result<(), String> {
        match &self.0 {
            Owner::Local(local) => local.relocate_execution_directory(candidate),
            Owner::Remote(remote) => remote.relocate_directory(candidate),
        }
    }
    pub(crate) fn close_observation(&self) -> super::CloseObservation {
        match &self.0 {
            Owner::Local(local) => local.close_observation(),
            Owner::Remote(remote) => remote.state().close.clone(),
        }
    }
    pub(crate) fn close_if_revision(&self, revision: u64) -> Result<(), String> {
        match &self.0 {
            Owner::Local(local) => local.close_if_revision(revision),
            Owner::Remote(remote) => {
                remote.close_method("close_if_revision", serde_json::json!([revision]))
            }
        }
    }
    pub(crate) fn close_if_idle(&self, generation: u64) -> Result<(), String> {
        match &self.0 {
            Owner::Local(local) => local.close_if_idle(generation),
            Owner::Remote(remote) => {
                remote.close_method("close_if_idle", serde_json::json!([generation]))
            }
        }
    }
    pub(crate) fn close_if_idle_for_resume(&self, generation: u64) -> Result<(), String> {
        match &self.0 {
            Owner::Local(local) => local.close_if_idle_for_resume(generation),
            Owner::Remote(remote) => {
                remote.close_method("close_if_idle", serde_json::json!([generation]))
            }
        }
    }
    pub(crate) fn check_update_readiness(&self) -> Result<UpdateReadiness, String> {
        match &self.0 {
            Owner::Local(local) => local.check_update_readiness(),
            Owner::Remote(remote) => remote.call("readiness", serde_json::Value::Null),
        }
    }
    pub(crate) fn interrupt_run(&self, run: crate::model::RunId) -> Result<(), String> {
        self.interrupt_owned(Some(run), false)
    }
    pub(crate) fn interrupt_owned(
        &self,
        run: Option<crate::model::RunId>,
        including_subagents: bool,
    ) -> Result<(), String> {
        match &self.0 {
            Owner::Local(local) => local.interrupt_owned(run, including_subagents),
            Owner::Remote(remote) => remote.interrupt(run, including_subagents),
        }
    }
    pub(crate) fn update_native_source(
        &self,
        source: chat::Session,
        execution_console: Option<crate::model::LaunchRecipe>,
        paired_holder: Option<crate::terminal::holder::Record>,
    ) -> Result<(), String> {
        match &self.0 {
            Owner::Local(local) => {
                local.update_native_source(source, execution_console, paired_holder)
            }
            Owner::Remote(remote) => remote.call(
                "update_source",
                serde_json::json!([source, execution_console, paired_holder]),
            ),
        }
    }
    pub fn set_visible(&self, visible: bool) {
        match &self.0 {
            Owner::Local(local) => local.set_visible(visible),
            Owner::Remote(remote) => remote.set_visible(visible),
        }
    }
    pub(crate) fn observe_next_update(&self) -> u64 {
        match &self.0 {
            Owner::Local(local) => local.observe_next_update(),
            Owner::Remote(remote) => remote.observe_next_update(),
        }
    }
    pub(crate) fn file_query(
        &self,
        command: &Command,
    ) -> Result<Option<chat::FileChangeReply>, String> {
        match &self.0 {
            Owner::Local(local) => local.file_query(command),
            Owner::Remote(remote) => remote.call("file_query", serde_json::json!([command])),
        }
    }
    pub(crate) fn provider_exit_output(
        &self,
        log_id: &str,
        offset: u64,
    ) -> Result<(Vec<u8>, Option<u64>, u64), String> {
        match &self.0 {
            Owner::Local(local) => local.provider_exit_output(log_id, offset),
            Owner::Remote(remote) => {
                remote.call("exit_output", serde_json::json!([log_id, offset]))
            }
        }
    }
    pub(crate) fn usage(&self) -> chat::Usage {
        match &self.0 {
            Owner::Local(local) => local.usage(),
            Owner::Remote(remote) => remote.state().usage.clone(),
        }
    }
    pub(crate) fn refresh_retained_state(&self) -> Result<(), String> {
        match &self.0 {
            Owner::Local(_) => Ok(()),
            Owner::Remote(remote) => remote.refresh_state(),
        }
    }
    pub(crate) fn continuity_loss(&self) -> Option<super::remote_actor::ContinuityLoss> {
        match &self.0 {
            Owner::Local(_) => None,
            Owner::Remote(remote) => remote.state().continuity_loss.clone(),
        }
    }
    pub(crate) fn ack_continuity_loss(&self, id: uuid::Uuid) -> Result<(), String> {
        match &self.0 {
            Owner::Local(_) => {
                Err("This local actor has no retained continuity loss receipt".into())
            }
            Owner::Remote(remote) => remote.call("ack_loss", serde_json::json!([id])),
        }
    }
    pub(crate) fn retained_runs(&self) -> Vec<crate::model::RunId> {
        match &self.0 {
            Owner::Local(_) => Vec::new(),
            Owner::Remote(remote) => remote.state().retained_runs.clone(),
        }
    }
    pub(crate) fn attachment_snapshot(&self) -> Snapshot {
        match &self.0 {
            Owner::Local(local) => local.snapshot(None).expect("unconditional local snapshot"),
            Owner::Remote(remote) => remote.attachment_snapshot(),
        }
    }
    pub fn snapshot(&self, after: Option<u64>) -> Option<Snapshot> {
        match &self.0 {
            Owner::Local(local) => local.snapshot(after),
            Owner::Remote(remote) => remote.snapshot(after),
        }
    }
    pub(crate) fn presentation(
        &self,
        chat_id: crate::model::TerminalId,
        after: Option<u64>,
    ) -> Option<crate::terminal_ipc::ServerResponse> {
        match &self.0 {
            Owner::Local(local) => local.presentation(chat_id, after),
            Owner::Remote(remote) => remote
                .call("presentation", serde_json::json!([chat_id, after]))
                .ok()
                .flatten(),
        }
    }
    pub fn session_id(&self) -> Option<String> {
        match &self.0 {
            Owner::Local(local) => local.session_id(),
            Owner::Remote(remote) => remote.state().session_id.clone(),
        }
    }
    pub(crate) fn confirmed_session_id(&self) -> Option<String> {
        match &self.0 {
            Owner::Local(local) => local.confirmed_session_id(),
            Owner::Remote(remote) => remote.state().confirmed_session.clone(),
        }
    }
    pub(crate) fn launch_account_scope(
        &self,
    ) -> Result<&crate::account::scope::LaunchAccountScope, &str> {
        match &self.0 {
            Owner::Local(local) => local.launch_account_scope(),
            Owner::Remote(remote) => remote.launch_account_scope(),
        }
    }
    pub(crate) fn execution_environment(&self) -> crate::terminal_ipc::ChatExecutionEnvironment {
        match &self.0 {
            Owner::Local(local) => local.execution_environment(),
            Owner::Remote(remote) => remote.execution_environment(),
        }
    }
    pub(crate) fn recovery_environment_matches(
        &self,
        environment: &BTreeMap<String, String>,
    ) -> bool {
        match &self.0 {
            Owner::Local(local) => local.recovery_environment_matches(environment),
            Owner::Remote(remote) => remote.recovery_environment_matches(environment),
        }
    }
    pub fn is_completed(&self) -> bool {
        match &self.0 {
            Owner::Local(local) => local.is_completed(),
            Owner::Remote(remote) => remote.state().completed,
        }
    }
    pub fn failure(&self) -> Option<String> {
        match &self.0 {
            Owner::Local(local) => local.failure(),
            Owner::Remote(remote) => remote.state().failure.clone(),
        }
    }
    pub fn owns_work(&self) -> bool {
        match &self.0 {
            Owner::Local(local) => local.owns_work(),
            Owner::Remote(remote) => remote.owns_work(),
        }
    }
    pub(crate) fn activity(&self) -> crate::agent::ActivityState {
        match &self.0 {
            Owner::Local(local) => local.activity(),
            Owner::Remote(remote) => remote.state().activity,
        }
    }
    pub(crate) fn update_activity(&self) -> (bool, bool) {
        match &self.0 {
            Owner::Local(local) => local.update_activity(),
            Owner::Remote(remote) => {
                let state = remote.state();
                (
                    state.working || state.waiting || !state.requests.is_empty(),
                    state.ready,
                )
            }
        }
    }
    pub(crate) fn is_idle(&self) -> bool {
        match &self.0 {
            Owner::Local(local) => local.is_idle(),
            Owner::Remote(remote) => remote.state().idle,
        }
    }
    pub(crate) fn requests(&self) -> Vec<chat::Request> {
        match &self.0 {
            Owner::Local(local) => local.requests(),
            Owner::Remote(remote) => remote.state().requests.clone(),
        }
    }
    pub(crate) fn background_subagents(&self) -> Vec<String> {
        match &self.0 {
            Owner::Local(local) => local.background_subagents(),
            Owner::Remote(remote) => remote.state().background_subagents.clone(),
        }
    }
    pub(crate) fn interrupt_subagents(&self) -> Vec<String> {
        match &self.0 {
            Owner::Local(local) => local.interrupt_subagents(),
            Owner::Remote(remote) => remote.state().interrupt_subagents.clone(),
        }
    }
    pub(crate) fn title(&self) -> Option<String> {
        match &self.0 {
            Owner::Local(local) => local.title(),
            Owner::Remote(remote) => remote.state().title.clone(),
        }
    }
    #[cfg(test)]
    pub(crate) fn resource_lifetime(&self) -> Box<dyn Fn() -> bool> {
        match &self.0 {
            Owner::Local(local) => Box::new(local.resource_lifetime()),
            Owner::Remote(remote) => remote.resource_lifetime(),
        }
    }
    pub(crate) fn finish_retained_close(&self) -> Result<(), String> {
        match &self.0 {
            Owner::Local(_) => Ok(()),
            Owner::Remote(remote) => remote.finish_close(),
        }
    }
    pub(crate) fn commit_native_installation(&self) -> Result<(), String> {
        match &self.0 {
            Owner::Local(local) => {
                local.commit_native_installation();
                Ok(())
            }
            Owner::Remote(remote) => remote.commit_installation(),
        }
    }
    pub(crate) fn native_continuity(&self) -> chat::NativeContinuity {
        match &self.0 {
            Owner::Local(local) => local.native_continuity(),
            Owner::Remote(remote) => remote.state().native_continuity.clone(),
        }
    }
    pub(crate) fn preserve_for_restart(&self) -> Result<(), String> {
        match &self.0 {
            Owner::Local(local) => local.preserve_for_restart(),
            Owner::Remote(remote) => remote.preserve(),
        }
    }
    pub(crate) fn close_for_resume(&self) -> Result<(), String> {
        match &self.0 {
            Owner::Local(local) => local.close_for_resume(),
            Owner::Remote(remote) => remote.close(),
        }
    }
    pub(crate) fn fence_session_stop_until(
        &self,
        actor_id: uuid::Uuid,
        deadline: std::time::Instant,
    ) -> Result<(), String> {
        match &self.0 {
            Owner::Local(local) => local.fence_session_stop_until(actor_id, deadline),
            Owner::Remote(remote) => remote.fence_session_stop_until(actor_id, deadline),
        }
    }
    pub(crate) fn stop_session_until(
        &self,
        actor_id: uuid::Uuid,
        deadline: std::time::Instant,
    ) -> Result<(), String> {
        self.fence_session_stop_until(actor_id, deadline)?;
        self.close_until(deadline)
    }
    pub(crate) fn close_until(&self, deadline: std::time::Instant) -> Result<(), String> {
        match &self.0 {
            Owner::Local(local) => local.close_until(deadline),
            Owner::Remote(remote) => remote.close_until(deadline),
        }
    }
    pub fn close(&self) -> Result<(), String> {
        match &self.0 {
            Owner::Local(local) => local.close(),
            Owner::Remote(remote) => remote.close(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn remote_ownership_is_authoritative_even_when_completion_is_cached() {
        let remote = RemoteActor::completed_failure_fixture();
        let handle = Handle(Owner::Remote(remote));
        assert!(handle.is_completed());
        assert!(handle.failure().is_some());
        assert!(handle.owns_work());
    }
}
