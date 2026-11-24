//! One handle for a daemon-local actor or its authenticated retained owner.
use super::retained_actor::journal::OutboxEntry;
use super::retained_actor::transport::{ActorIdentity, RemoteActor};
use super::{LocalHandle, Rejection, Store, TitleUnavailable, Update, UpdateReadiness};
use crate::chat::{
    self, Command, Snapshot,
    protocol::{Adapter, ProcessSpec, Step},
};
use std::collections::BTreeMap;
#[derive(Clone)]
pub struct Handle(Owner);
/// Why a read of the conversation or its changed files failed.
#[derive(Debug)]
pub struct ReadFailure {
    pub message: String,
    /// Whether the conversation's owner was alive when the read failed, so
    /// reading again can succeed. For the transcript the owner then asks the
    /// reader to.
    pub transient: bool,
}
impl From<String> for ReadFailure {
    /// A refusal, which reading again would repeat.
    fn from(message: String) -> Self {
        Self {
            message,
            transient: false,
        }
    }
}
#[derive(Clone)]
enum Owner {
    Local(LocalHandle),
    Remote(RemoteActor),
}
/// Outcome observed after a fixture proxy has admitted an exact envelope.
#[cfg(any(test, feature = "test-support"))]
pub enum RetainedInputOutcome {
    Acknowledged(Result<(), String>),
    Unknown(String),
    Unavailable,
    Disconnected,
}

impl Handle {
    /// A real remote proxy with no external provider or helper. The returned
    /// responder waits for actual production admission before settling it.
    #[cfg(any(test, feature = "test-support"))]
    pub fn fixture_retained_input() -> (
        Self,
        impl FnOnce(RetainedInputOutcome) -> u64 + Send + 'static,
    ) {
        let remote = RemoteActor::fixture();
        let responder = remote.clone();
        (Self(Owner::Remote(remote)), move |outcome| {
            responder.settle_input_fixture(outcome)
        })
    }
    /// A retained attachment for `source` whose helper is a script the test
    /// edits; see [`ScriptedHelper`](super::retained_actor::transport::scripted_helper::ScriptedHelper).
    #[cfg(any(test, feature = "test-support"))]
    pub fn fixture_scripted(
        source: &crate::chat::Session,
        scope: Result<crate::account::scope::LaunchAccountScope, String>,
    ) -> (
        Self,
        super::retained_actor::transport::scripted_helper::ScriptedHelper,
    ) {
        let (remote, helper) =
            super::retained_actor::transport::scripted_helper::ScriptedHelper::start(source, scope);
        (Self(Owner::Remote(remote)), helper)
    }
    /// A retained attachment whose proxy already recorded a terminal failure.
    #[cfg(any(test, feature = "test-support"))]
    pub fn fixture_failed_remote() -> Self {
        Self(Owner::Remote(RemoteActor::completed_failure_fixture()))
    }
    /// Model the provider naming its conversation after it started, as a new
    /// one does.
    #[cfg(any(test, feature = "test-support"))]
    pub fn fixture_publish_session(&self, session: String) {
        match &self.0 {
            Owner::Remote(remote) => remote.publish_session_fixture(session),
            Owner::Local(_) => panic!("session publication fixture requires a retained actor"),
        }
    }
    /// Model the provider confirming its conversation after this attachment.
    #[cfg(any(test, feature = "test-support"))]
    pub fn fixture_confirm_session(&self, session: String) {
        match &self.0 {
            Owner::Remote(remote) => remote.confirm_session_fixture(session),
            Owner::Local(_) => panic!("session confirmation fixture requires a retained actor"),
        }
    }
    /// Same retained actor identity, distinct proxy attachment allocations.
    /// Neither fixture has a live helper or permits real provider operations.
    #[cfg(any(test, feature = "test-support"))]
    pub fn fixture_remote_attachment_pair() -> (Self, Self) {
        let first = RemoteActor::fixture();
        let second = first.distinct_attachment_fixture();
        (Self(Owner::Remote(first)), Self(Owner::Remote(second)))
    }
    #[cfg(any(test, feature = "test-support"))]
    pub fn take_pending_visibility_fixture(&self) -> Option<bool> {
        match &self.0 {
            Owner::Remote(remote) => remote.take_pending_visibility_fixture(),
            Owner::Local(_) => panic!("visibility fixture requires a retained actor"),
        }
    }
    #[cfg(any(test, feature = "test-support"))]
    pub fn local_command_queue_is_empty(&self) -> bool {
        match &self.0 {
            Owner::Local(local) => local.0.commands.is_empty(),
            Owner::Remote(_) => panic!("courier barrier fixture requires a local actor"),
        }
    }
    #[cfg(any(test, feature = "test-support"))]
    pub fn start(
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
    pub fn start_owned(
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
            notify,
        )
        .map(|local| Self(Owner::Local(local)))
    }
    pub fn start_adopted(
        spec: ProcessSpec,
        adapter: Box<dyn Adapter>,
        initial: Step,
        store: Store,
        recovery: chat::native_holder::Recovery,
        holder: chat::native_holder::Holder,
        notify: impl Fn(Update) + Send + Sync + 'static,
    ) -> Result<Self, String> {
        LocalHandle::start_adopted(spec, adapter, initial, store, recovery, holder, notify)
            .map(|local| Self(Owner::Local(local)))
    }
    pub fn start_remote_owned(
        spec: ProcessSpec,
        source: chat::Session,
        prepared_identity: chat::protocol::claude::PreparedIdentity,
        recovery_environment: BTreeMap<String, String>,
        notify: impl Fn(Update) + Send + Sync + 'static,
    ) -> Result<Self, String> {
        RemoteActor::spawn(
            spec,
            source,
            prepared_identity,
            recovery_environment,
            notify,
        )
        .map(|remote| Self(Owner::Remote(remote)))
    }
    pub fn adopt_remote(
        record: &crate::daemon::holder_registry::Record,
        source: chat::Session,
        proof: crate::model::HostSessionProof,
        notify: impl Fn(Update) + Send + Sync + 'static,
    ) -> Result<Self, String> {
        RemoteActor::adopt(record, source, proof, notify).map(|remote| Self(Owner::Remote(remote)))
    }
    pub fn adopt_remote_for_close(
        record: &crate::daemon::holder_registry::Record,
        source: chat::Session,
        proof: crate::model::HostSessionProof,
        notify: impl Fn(Update) + Send + Sync + 'static,
    ) -> Result<Self, String> {
        RemoteActor::adopt_for_close(record, source, proof, notify)
            .map(|remote| Self(Owner::Remote(remote)))
    }
    /// Lets the daemon open, with `recognizer`, the frames it sealed in what
    /// a retained helper built before them shows as a person's prompts. A
    /// conversation this process owns opens them itself.
    pub fn open_frames_for_older_helper(
        &self,
        recognizer: &std::sync::Arc<crate::agent_execution::mailbox::Recognizer>,
    ) {
        if let Owner::Remote(remote) = &self.0 {
            remote.open_frames_for_older_helper(recognizer);
        }
    }
    pub fn is_close_only(&self) -> bool {
        matches!(&self.0, Owner::Remote(remote) if remote.is_close_only())
    }
    pub fn inspect_remote(
        record: &crate::daemon::holder_registry::Record,
        proof: crate::model::HostSessionProof,
    ) -> Result<super::retained_actor::transport::ActorInspection, String> {
        RemoteActor::inspect(record, proof)
    }
    pub fn inspect_remote_until(
        record: &crate::daemon::holder_registry::Record,
        proof: crate::model::HostSessionProof,
        deadline: std::time::Instant,
    ) -> Result<super::retained_actor::transport::ActorInspection, String> {
        RemoteActor::inspect_until(record, proof, deadline)
    }
    pub fn retained_close_proved(
        &self,
        source: &chat::Session,
        proof: &crate::model::HostSessionProof,
    ) -> Result<bool, String> {
        match &self.0 {
            Owner::Remote(remote) => RemoteActor::close_proved(&remote.identity(), proof, source),
            Owner::Local(_) => Ok(false),
        }
    }
    pub fn inspect_retained_retirement(
        record: &crate::daemon::holder_registry::Record,
        proof: &crate::model::HostSessionProof,
    ) -> Result<super::RetirementInspection, String> {
        RemoteActor::inspect_retained_retirement(record, proof)
    }
    pub fn inspect_abandoned_actor(
        record: &crate::daemon::holder_registry::Record,
        proof: &crate::model::HostSessionProof,
    ) -> Result<bool, String> {
        RemoteActor::inspect_abandoned(record, proof)
    }
    #[cfg(any(test, feature = "test-support"))]
    pub fn retired_actor_receipt(
        record: &crate::daemon::holder_registry::Record,
        proof: &crate::model::HostSessionProof,
    ) -> Result<Option<super::retained_actor::transport::RetirementReceipt>, String> {
        RemoteActor::retired_actor_receipt(record, proof)
    }
    pub fn compact_retired_receipt(
        record: &crate::daemon::holder_registry::Record,
        proof: &crate::model::HostSessionProof,
    ) -> Result<u64, String> {
        RemoteActor::compact_retired_receipt(record, proof)
    }
    pub fn is_remote_record(record: &crate::daemon::holder_registry::Record) -> bool {
        RemoteActor::is_record(record)
    }
    pub fn wake_retained_outcomes(&self) {
        if let Owner::Remote(remote) = &self.0 {
            remote.wake_outcomes();
        }
    }
    /// See [`RemoteActor::uncommitted_outcomes`].
    pub fn uncommitted_outcomes(&self) -> Result<Vec<OutboxEntry>, String> {
        match &self.0 {
            Owner::Remote(remote) => remote.uncommitted_outcomes(),
            _ => Err("This actor has no retained outcome outbox".into()),
        }
    }
    pub fn retained_outcome_clock_until(
        &self,
        deadline: std::time::Instant,
    ) -> Result<(u64, u64), String> {
        let Owner::Remote(remote) = &self.0 else {
            return Err("This actor has no retained outcome outbox".into());
        };
        remote.outcome_clock_until(deadline)
    }
    /// See [`RemoteActor::uncommitted_outcomes_until`].
    pub fn uncommitted_outcomes_until(
        &self,
        deadline: std::time::Instant,
    ) -> Result<Vec<OutboxEntry>, String> {
        match &self.0 {
            Owner::Remote(remote) => remote.uncommitted_outcomes_until(deadline),
            _ => Err("This actor has no retained outcome outbox".into()),
        }
    }
    pub fn recognize_retained_sources_until(
        &self,
        sources: &[chat::Session],
        deadline: std::time::Instant,
    ) -> Result<(), String> {
        match &self.0 {
            Owner::Remote(remote) => {
                remote.call_until("recognize_sources", serde_json::json!([sources]), deadline)
            }
            _ => Err("This actor has no retained source authority".into()),
        }
    }
    #[cfg(any(test, feature = "test-support"))]
    pub fn provider_process(&self) -> Option<crate::daemon::holder_registry::Process> {
        match &self.0 {
            Owner::Local(local) => local.provider_process(),
            Owner::Remote(remote) => remote.state().provider_process.clone(),
        }
    }
    pub fn actor_identity(&self) -> Option<ActorIdentity> {
        match &self.0 {
            Owner::Remote(remote) => Some(remote.identity()),
            _ => None,
        }
    }
    pub fn commit_retained_outcomes(&self, through: u64) -> Result<(), String> {
        match &self.0 {
            Owner::Remote(remote) => remote.call("ack_outbox", serde_json::json!([through])),
            _ => Err("This actor has no remote outbox".into()),
        }
    }
    pub fn commit_retained_outcomes_until(
        &self,
        through: u64,
        deadline: std::time::Instant,
    ) -> Result<(), String> {
        match &self.0 {
            Owner::Remote(remote) => {
                remote.call_until("ack_outbox", serde_json::json!([through]), deadline)
            }
            _ => Err("This actor has no retained outcome outbox".into()),
        }
    }
    #[cfg(any(test, feature = "test-support"))]
    pub fn pending_retained_inputs(&self) -> Vec<super::retained_actor::journal::InputIdentity> {
        match &self.0 {
            Owner::Remote(remote) => remote.pending_inputs(),
            _ => Vec::new(),
        }
    }
    pub fn same_actor(&self, other: &Self) -> bool {
        match (&self.0, &other.0) {
            (Owner::Local(a), Owner::Local(b)) => a.same_actor(b),
            (Owner::Remote(a), Owner::Remote(b)) => a.identity() == b.identity(),
            _ => false,
        }
    }
    /// Identity alone survives epoch changes; grants retain this exact proxy.
    pub fn same_attachment(&self, other: &Self) -> bool {
        match (&self.0, &other.0) {
            (Owner::Local(a), Owner::Local(b)) => a.same_actor(b),
            (Owner::Remote(a), Owner::Remote(b)) => a.same_attachment(b),
            _ => false,
        }
    }
    pub fn resolve_retained_source(&self, source: &chat::Session) -> Result<chat::Session, String> {
        match &self.0 {
            Owner::Remote(remote) => remote.resolve_source(source),
            _ => Err("This actor has no retained source resolver".into()),
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
    /// Delivers work through the daemon's ledger, which outlives this owner.
    pub fn deliver(
        &self,
        deliveries: &super::Deliveries,
        chat: crate::model::TerminalId,
        command: Command,
        key: chat::DeliveryKey,
        given_work: impl FnOnce(),
    ) -> Result<(), String> {
        match &self.0 {
            Owner::Local(local) => local.deliver(deliveries, chat, command, key, given_work),
            Owner::Remote(remote) => remote.deliver(deliveries, chat, command, key, given_work),
        }
        .map_err(String::from)
    }
    pub fn try_idle_prompt(&self, text: String) -> Result<(), Rejection> {
        match &self.0 {
            Owner::Local(local) => local.try_idle_prompt(text),
            Owner::Remote(remote) => remote.try_idle_prompt(text),
        }
    }
    /// Exact source recognition is checked by the helper under this attachment epoch.
    pub fn recognize_retained_sources(&self, sources: &[chat::Session]) -> Result<(), String> {
        match &self.0 {
            Owner::Remote(remote) => remote.call("recognize_sources", serde_json::json!([sources])),
            Owner::Local(_) => Err("This actor has no retained source authority".into()),
        }
    }
    pub fn command_for_run(
        &self,
        run: crate::model::RunId,
        command: Command,
    ) -> Result<(), String> {
        match &self.0 {
            Owner::Local(local) => local.command_for_run(run, command),
            Owner::Remote(remote) => remote.submit(Some(run), command).map_err(String::from),
        }
    }
    pub fn close_if_idle_for_resume(&self, generation: u64) -> Result<(), String> {
        match &self.0 {
            Owner::Local(local) => local.close_if_idle_for_resume(generation),
            Owner::Remote(remote) => {
                remote.close_method("close_if_idle", serde_json::json!([generation]))
            }
        }
    }
    pub fn check_update_readiness(&self) -> Result<UpdateReadiness, String> {
        match &self.0 {
            Owner::Local(local) => local.check_update_readiness(),
            Owner::Remote(remote) => remote.call("readiness", serde_json::Value::Null),
        }
    }
    pub fn interrupt_run(&self, run: crate::model::RunId) -> Result<(), String> {
        self.interrupt_owned(Some(run), false)
    }
    pub fn interrupt_owned(
        &self,
        run: Option<crate::model::RunId>,
        including_subagents: bool,
    ) -> Result<(), String> {
        match &self.0 {
            Owner::Local(local) => local.interrupt_owned(run, including_subagents),
            Owner::Remote(remote) => remote.interrupt(run, including_subagents),
        }
    }
    pub fn update_native_source(
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
    pub fn observe_next_update(&self) -> u64 {
        match &self.0 {
            Owner::Local(local) => local.observe_next_update(),
            Owner::Remote(remote) => remote.observe_next_update(),
        }
    }
    /// Answer a changed files query from the cached edit metadata. Only
    /// [`Command::queries_file_changes`] commands are queries.
    pub fn file_query(&self, command: &Command) -> Result<chat::FileChangeReply, ReadFailure> {
        let reply = match &self.0 {
            Owner::Local(local) => local.file_query(command)?,
            Owner::Remote(remote) => remote.file_query(command)?,
        };
        reply.ok_or_else(|| ReadFailure::from("Expected a changed files query".to_string()))
    }
    /// Copy is served by the same Store which published the immutable source.
    pub fn text_copy(
        &self,
        source: &chat::TextCopySource,
        offset: u64,
    ) -> Result<chat::TextCopyPage, String> {
        match &self.0 {
            Owner::Local(local) => local.text_copy(source, offset),
            Owner::Remote(remote) => remote.call("text_copy", serde_json::json!([source, offset])),
        }
    }
    pub fn history_is_covered_by(&self, reloaded: &Store) -> Result<bool, String> {
        match &self.0 {
            Owner::Local(local) => local.history_is_covered_by(reloaded),
            Owner::Remote(_) => Err(
                "Retained provider ownership requires authenticated close before release".into(),
            ),
        }
    }
    pub fn is_read_only_history(&self) -> bool {
        matches!(&self.0, Owner::Local(local) if local.is_read_only_history())
    }
    pub fn resident_generation(&self) -> u64 {
        match &self.0 {
            Owner::Local(local) => {
                local
                    .0
                    .state
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .generation
            }
            Owner::Remote(remote) => remote.state().generation,
        }
    }
    pub fn try_close_if_idle(&self, generation: u64) -> Result<bool, String> {
        match &self.0 {
            Owner::Local(local) => local.try_close_if_idle(generation),
            Owner::Remote(remote) => remote.try_close_if_idle(generation),
        }
    }
    /// Ends an idle conversation's provider when `generation` is still
    /// current, with `detached` deciding what it leaves running, as a close
    /// does. False means the conversation changed and nothing stopped.
    pub fn try_stop_if_idle(
        &self,
        generation: u64,
        detached: crate::daemon::owned_children::DetachedWork,
    ) -> Result<bool, String> {
        match &self.0 {
            Owner::Local(local) => local.try_stop_if_idle(generation, detached),
            Owner::Remote(remote) => remote.try_stop_if_idle(generation, detached),
        }
    }
    pub fn usage(&self) -> chat::Usage {
        match &self.0 {
            Owner::Local(local) => local.usage(),
            Owner::Remote(remote) => remote.state().usage.clone(),
        }
    }
    pub fn refresh_retained_state(&self) -> Result<(), String> {
        match &self.0 {
            Owner::Local(_) => Ok(()),
            Owner::Remote(remote) => remote.refresh_state(),
        }
    }
    pub fn continuity_loss(&self) -> Option<super::retained_actor::journal::ContinuityLoss> {
        match &self.0 {
            Owner::Local(_) => None,
            Owner::Remote(remote) => remote.state().continuity_loss.clone(),
        }
    }
    pub fn ack_continuity_loss(&self, id: uuid::Uuid) -> Result<(), String> {
        match &self.0 {
            Owner::Local(_) => {
                Err("This local actor has no retained continuity loss receipt".into())
            }
            Owner::Remote(remote) => remote.call("ack_loss", serde_json::json!([id])),
        }
    }
    pub fn retained_runs(&self) -> Vec<crate::model::RunId> {
        match &self.0 {
            Owner::Local(_) => Vec::new(),
            Owner::Remote(remote) => remote.state().retained_runs.clone(),
        }
    }
    #[cfg(any(test, feature = "test-support"))]
    pub fn attachment_snapshot(&self) -> Snapshot {
        match &self.0 {
            Owner::Local(local) => local.snapshot(None).expect("unconditional local snapshot"),
            Owner::Remote(remote) => remote.attachment_snapshot(),
        }
    }
    /// The items changed since the last call, whole, and whether the
    /// conversation is working; see [`super::Unread`]. Only a conversation
    /// this process owns is followed this way; a retained actor keeps its
    /// record in another process.
    pub fn unread(&self, again: &[String]) -> Result<(super::Unread, bool), String> {
        match &self.0 {
            Owner::Local(local) => local.unread(again),
            Owner::Remote(_) => Err("it is kept by another process".into()),
        }
    }
    pub fn snapshot(&self, after: Option<u64>) -> Option<Snapshot> {
        match &self.0 {
            Owner::Local(local) => local.snapshot(after),
            Owner::Remote(remote) => remote.snapshot(after),
        }
    }
    pub fn presentation_wire(
        &self,
        chat_id: crate::model::TerminalId,
        after: Option<u64>,
    ) -> Result<crate::terminal_ipc::PreparedResponse, ReadFailure> {
        match &self.0 {
            Owner::Local(local) => Ok(local
                .presentation(chat_id, after)
                .unwrap_or(crate::terminal_ipc::ServerResponse::ChatSnapshot {
                    chat_id,
                    snapshot: None,
                })
                .into()),
            Owner::Remote(remote) => remote.presentation_wire(chat_id, after),
        }
    }
    pub fn session_id(&self) -> Option<String> {
        match &self.0 {
            Owner::Local(local) => local.session_id(),
            Owner::Remote(remote) => remote.state().session_id.clone(),
        }
    }
    pub fn is_ready(&self) -> bool {
        match &self.0 {
            Owner::Local(local) => local.is_ready(),
            Owner::Remote(remote) => remote.state().ready,
        }
    }
    /// How long a server this conversation launched has waited behind another
    /// process's file lock. A retained remote actor launches none here.
    pub fn startup_lock_wait(&self) -> std::time::Duration {
        match &self.0 {
            Owner::Local(local) => local.startup_lock_wait(),
            Owner::Remote(_) => std::time::Duration::ZERO,
        }
    }
    pub fn confirmed_session_id(&self) -> Option<String> {
        match &self.0 {
            Owner::Local(local) => local.confirmed_session_id(),
            Owner::Remote(remote) => remote.state().confirmed_session.clone(),
        }
    }
    pub fn launch_account_scope(&self) -> Result<&crate::account::scope::LaunchAccountScope, &str> {
        match &self.0 {
            Owner::Local(local) => local.launch_account_scope(),
            Owner::Remote(remote) => remote.launch_account_scope(),
        }
    }
    pub fn execution_environment(&self) -> crate::terminal_ipc::ChatExecutionEnvironment {
        match &self.0 {
            Owner::Local(local) => local.execution_environment(),
            Owner::Remote(remote) => remote.execution_environment(),
        }
    }
    pub fn recovery_environment_matches(&self, environment: &BTreeMap<String, String>) -> bool {
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
    /// When the latest turn finished, in daemon Unix milliseconds, which
    /// dates the failure it may have ended with.
    pub fn finished_at(&self) -> Option<u64> {
        match &self.0 {
            Owner::Local(local) => local.finished_at(),
            Owner::Remote(remote) => remote.state().finished_at,
        }
    }
    /// The conversation's prompt cache activity, and the context its next
    /// request reads, as of the latest observation.
    pub fn prompt_cache(&self) -> (chat::prompt_cache::Activity, u64) {
        match &self.0 {
            Owner::Local(local) => (local.prompt_cache(), local.context_tokens()),
            Owner::Remote(remote) => {
                let state = remote.state();
                (state.prompt_cache.clone(), state.usage.context_tokens)
            }
        }
    }
    pub fn owns_work(&self) -> bool {
        match &self.0 {
            Owner::Local(local) => local.owns_work(),
            Owner::Remote(remote) => remote.owns_work(),
        }
    }
    pub fn activity(&self) -> crate::agent::ActivityState {
        match &self.0 {
            Owner::Local(local) => local.activity(),
            Owner::Remote(remote) => remote.state().activity,
        }
    }
    /// Whether the conversation's own turn is under way or awaiting the user,
    /// apart from its background children.
    pub fn turn_accepted(&self) -> bool {
        match &self.0 {
            Owner::Local(local) => local.turn_accepted(),
            Owner::Remote(remote) => {
                let state = remote.state();
                state.busy || state.waiting || !state.requests.is_empty()
            }
        }
    }
    pub fn update_activity(&self) -> (bool, bool) {
        match &self.0 {
            Owner::Local(local) => local.update_activity(),
            Owner::Remote(remote) => {
                let state = remote.state();
                (
                    state.working
                        || state.waiting
                        || !state.requests.is_empty()
                        || !state.work_hold().subagents.is_empty(),
                    state.ready,
                )
            }
        }
    }
    /// What holds the conversation, in the terms its reader knows it by.
    pub fn work_hold(&self) -> chat::WorkHold {
        match &self.0 {
            Owner::Local(local) => local.work_hold(),
            Owner::Remote(remote) => remote.state().work_hold(),
        }
    }
    pub fn is_idle(&self) -> bool {
        match &self.0 {
            Owner::Local(local) => local.is_idle(),
            Owner::Remote(remote) => remote.state().idle,
        }
    }
    /// The provider-confirmed permission mode. A local actor reads it
    /// without publishing a snapshot.
    pub fn permission_mode(&self) -> Option<String> {
        match &self.0 {
            Owner::Local(local) => local.permission_mode(),
            Owner::Remote(remote) => remote
                .snapshot(None)
                .and_then(|snapshot| snapshot.permission_mode),
        }
    }
    pub fn requests(&self) -> Vec<chat::Request> {
        match &self.0 {
            Owner::Local(local) => local.requests(),
            Owner::Remote(remote) => remote.state().requests.clone(),
        }
    }
    pub fn background_subagents(&self) -> Vec<String> {
        match &self.0 {
            Owner::Local(local) => local.background_subagents(),
            Owner::Remote(remote) => remote.state().background_subagents.clone(),
        }
    }
    /// How many background tasks the provider reports live, or `None` from
    /// a helper that predates reporting them.
    pub fn background_tasks(&self) -> Option<usize> {
        match &self.0 {
            Owner::Local(local) => Some(local.background_tasks()),
            Owner::Remote(remote) => remote.state().background_tasks,
        }
    }
    pub fn interrupt_subagents(&self) -> Vec<String> {
        match &self.0 {
            Owner::Local(local) => local.interrupt_subagents(),
            Owner::Remote(remote) => remote.state().interrupt_subagents.clone(),
        }
    }
    /// Provider-confirmed model status from the local or retained owner.
    pub fn active_model(&self) -> (u64, Option<chat::ActiveModel>) {
        match &self.0 {
            Owner::Local(local) => local.active_model(),
            Owner::Remote(remote) => {
                let state = remote.state();
                (state.active_model_revision, state.active_model.clone())
            }
        }
    }
    /// How the provider addresses the conversation's subagents, where that
    /// is not by the IDs its snapshot lists them under.
    pub fn agent_addresses(&self) -> Vec<chat::AgentAddress> {
        match &self.0 {
            Owner::Local(local) => local.agent_addresses(),
            Owner::Remote(remote) => remote.state().agent_addresses.clone(),
        }
    }
    pub fn title(&self) -> Option<String> {
        match &self.0 {
            Owner::Local(local) => local.title(),
            Owner::Remote(remote) => remote.state().title.clone(),
        }
    }
    #[cfg(any(test, feature = "test-support"))]
    pub fn resource_lifetime(&self) -> Box<dyn Fn() -> bool> {
        match &self.0 {
            Owner::Local(local) => Box::new(local.resource_lifetime()),
            Owner::Remote(remote) => remote.resource_lifetime(),
        }
    }
    pub fn finish_retained_close(&self) -> Result<(), String> {
        match &self.0 {
            Owner::Local(_) => Ok(()),
            Owner::Remote(remote) => remote.finish_close(),
        }
    }
    pub fn commit_native_installation(&self) -> Result<(), String> {
        match &self.0 {
            Owner::Local(local) => {
                local.commit_native_installation();
                Ok(())
            }
            Owner::Remote(remote) => remote.commit_installation(),
        }
    }
    pub fn native_continuity(&self) -> chat::NativeContinuity {
        match &self.0 {
            Owner::Local(local) => local.native_continuity(),
            Owner::Remote(remote) => remote.state().native_continuity.clone(),
        }
    }
    pub fn preserve_for_restart(&self) -> Result<(), String> {
        match &self.0 {
            Owner::Local(local) => local.preserve_for_restart(),
            Owner::Remote(remote) => remote.preserve(),
        }
    }
    /// Decides, before any close, whether it leaves running the work the
    /// provider had already detached from itself, such as a server a command
    /// started with `nohup`. Closes that have not decided stop it.
    pub fn decide_detached_work(&self, detached: crate::daemon::owned_children::DetachedWork) {
        match &self.0 {
            Owner::Local(local) => local.decide_detached_work(detached),
            Owner::Remote(remote) => remote.decide_detached_work(detached),
        }
    }
    pub fn close_for_resume(&self) -> Result<(), String> {
        match &self.0 {
            Owner::Local(local) => local.close_for_resume(),
            Owner::Remote(remote) => remote.close(),
        }
    }
    pub fn close(&self) -> Result<(), String> {
        match &self.0 {
            Owner::Local(local) => local.close(),
            Owner::Remote(remote) => remote.close(),
        }
    }
    pub fn decline_answer(&self, command: Command) -> Result<(), String> {
        match &self.0 {
            Owner::Local(local) => local.decline_answer(command),
            Owner::Remote(remote) => remote.decline_answer(command),
        }
    }
    #[cfg(any(test, feature = "test-support"))]
    pub fn conversation_title(&self) -> Result<(Option<String>, Option<String>), TitleUnavailable> {
        match &self.0 {
            Owner::Local(local) => local.conversation_title(),
            Owner::Remote(remote) => remote.conversation_title(),
        }
    }
    pub fn conversation_naming(&self) -> Result<super::ConversationNaming, TitleUnavailable> {
        match &self.0 {
            Owner::Local(local) => local.conversation_naming(),
            Owner::Remote(remote) => remote.conversation_naming(),
        }
    }
    #[cfg(any(test, feature = "test-support"))]
    pub fn fixture_retained_native_holder(
        &self,
        holder: chat::native_holder::Holder,
        reason: String,
    ) {
        match &self.0 {
            Owner::Local(local) => local.fixture_retained_native_holder(holder, reason),
            Owner::Remote(_) => panic!("local Codex holder fixture requires a local actor"),
        }
    }
}
impl chat::protocol::LaunchCleanup for Handle {
    fn cleanup(&self) -> Result<chat::protocol::LaunchOutcome, String> {
        match &self.0 {
            Owner::Local(local) => local.cleanup(),
            Owner::Remote(remote) => remote.cleanup(),
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn attachment_identity_distinguishes_new_proxy_epochs_of_the_same_actor() {
        let remote = RemoteActor::fixture();
        let distinct = remote.distinct_attachment_fixture();
        assert_eq!(remote.identity(), distinct.identity());
        let handle = Handle(Owner::Remote(remote));
        let clone = handle.clone();
        let next = Handle(Owner::Remote(distinct));
        assert!(handle.same_actor(&next));
        assert!(handle.same_attachment(&clone));
        assert!(!handle.same_attachment(&next));
    }
    #[test]
    fn remote_ownership_is_authoritative_even_when_completion_is_cached() {
        let remote = RemoteActor::completed_failure_fixture();
        let handle = Handle(Owner::Remote(remote));
        assert!(handle.is_completed());
        assert!(handle.failure().is_some());
        assert!(handle.owns_work());
    }
}
