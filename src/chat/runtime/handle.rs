//! One handle for a daemon-local actor or its authenticated retained owner.
use super::retained_actor::journal::{InputIdentity, InputReceipt, OutboxEntry};
use super::retained_actor::transport::{ActorIdentity, RemoteActor};
use super::{LocalHandle, Rejection, Store, TitleUnavailable, Update, UpdateReadiness};
use crate::chat::{
    self, Command, Snapshot,
    protocol::{Adapter, ProcessSpec, Step},
};
use std::collections::BTreeMap;
#[derive(Clone)]
pub struct Handle(Owner);
#[derive(Clone)]
enum Owner {
    Local(LocalHandle),
    Remote(RemoteActor),
}
/// Outcome observed after a fixture proxy has admitted an exact envelope.
#[cfg(test)]
pub(crate) enum RetainedInputOutcome {
    Acknowledged(Result<(), String>),
    Unknown(String),
    Unavailable,
    Disconnected,
}

impl Handle {
    #[cfg(test)]
    pub(in crate::chat::runtime) fn fixture_peer_page_pair(
        local: LocalHandle,
        remote: RemoteActor,
    ) -> (Self, Self) {
        (Self(Owner::Local(local)), Self(Owner::Remote(remote)))
    }
    /// A real remote proxy with no external provider or helper. The returned
    /// responder waits for actual production admission before settling it.
    #[cfg(test)]
    pub(crate) fn fixture_retained_input() -> (
        Self,
        impl FnOnce(RetainedInputOutcome) -> u64 + Send + 'static,
    ) {
        let remote = RemoteActor::fixture();
        let responder = remote.clone();
        (Self(Owner::Remote(remote)), move |outcome| {
            responder.settle_input_fixture(outcome)
        })
    }
    /// Same retained actor identity, distinct proxy attachment allocations.
    /// Neither fixture has a live helper or permits real provider operations.
    #[cfg(test)]
    pub(crate) fn fixture_remote_attachment_pair() -> (Self, Self) {
        let first = RemoteActor::fixture();
        let second = first.distinct_attachment_fixture();
        (Self(Owner::Remote(first)), Self(Owner::Remote(second)))
    }
    #[cfg(test)]
    pub(crate) fn local_command_queue_is_empty(&self) -> bool {
        match &self.0 {
            Owner::Local(local) => local.0.commands.is_empty(),
            Owner::Remote(_) => panic!("courier barrier fixture requires a local actor"),
        }
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
        LocalHandle::start_adopted(spec, adapter, initial, store, recovery, holder, notify)
            .map(|local| Self(Owner::Local(local)))
    }
    pub(crate) fn start_remote_owned(
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
    pub(crate) fn adopt_remote(
        record: &crate::daemon::holder_registry::Record,
        source: chat::Session,
        proof: crate::model::HostSessionProof,
        notify: impl Fn(Update) + Send + Sync + 'static,
    ) -> Result<Self, String> {
        RemoteActor::adopt(record, source, proof, notify).map(|remote| Self(Owner::Remote(remote)))
    }
    pub(crate) fn adopt_remote_for_close(
        record: &crate::daemon::holder_registry::Record,
        source: chat::Session,
        proof: crate::model::HostSessionProof,
        notify: impl Fn(Update) + Send + Sync + 'static,
    ) -> Result<Self, String> {
        RemoteActor::adopt_for_close(record, source, proof, notify)
            .map(|remote| Self(Owner::Remote(remote)))
    }
    pub(crate) fn is_close_only(&self) -> bool {
        matches!(&self.0, Owner::Remote(remote) if remote.is_close_only())
    }
    pub(crate) fn inspect_remote(
        record: &crate::daemon::holder_registry::Record,
        proof: crate::model::HostSessionProof,
    ) -> Result<super::retained_actor::transport::ActorInspection, String> {
        RemoteActor::inspect(record, proof)
    }
    pub(crate) fn inspect_remote_until(
        record: &crate::daemon::holder_registry::Record,
        proof: crate::model::HostSessionProof,
        deadline: std::time::Instant,
    ) -> Result<super::retained_actor::transport::ActorInspection, String> {
        RemoteActor::inspect_until(record, proof, deadline)
    }
    pub(crate) fn retained_close_proved(
        &self,
        source: &chat::Session,
        proof: &crate::model::HostSessionProof,
    ) -> Result<bool, String> {
        match &self.0 {
            Owner::Remote(remote) => RemoteActor::close_proved(&remote.identity(), proof, source),
            Owner::Local(_) => Ok(false),
        }
    }
    pub(crate) fn inspect_retained_retirement(
        record: &crate::daemon::holder_registry::Record,
        proof: &crate::model::HostSessionProof,
    ) -> Result<super::RetirementInspection, String> {
        RemoteActor::inspect_retained_retirement(record, proof)
    }
    pub(crate) fn retired_actor_receipt(
        record: &crate::daemon::holder_registry::Record,
        proof: &crate::model::HostSessionProof,
    ) -> Result<Option<super::retained_actor::transport::RetirementReceipt>, String> {
        RemoteActor::retired_actor_receipt(record, proof)
    }
    pub(crate) fn is_remote_record(record: &crate::daemon::holder_registry::Record) -> bool {
        RemoteActor::is_record(record)
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
    pub(crate) fn retained_outcome_clock_until(
        &self,
        deadline: std::time::Instant,
    ) -> Result<(u64, u64), String> {
        let Owner::Remote(remote) = &self.0 else {
            return Err("This actor has no retained outcome outbox".into());
        };
        remote.outcome_clock_until(deadline)
    }
    pub(crate) fn uncommitted_outcomes_until(
        &self,
        deadline: std::time::Instant,
    ) -> Result<Vec<OutboxEntry>, String> {
        match &self.0 {
            Owner::Remote(remote) => {
                remote.call_until("outbox_unacked", serde_json::Value::Null, deadline)
            }
            _ => Err("This actor has no retained outcome outbox".into()),
        }
    }
    pub(crate) fn recognize_retained_sources_until(
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
    /// Physical launch-directory evidence. The service caller must separately
    /// validate that this is still its current exact actor before using it.
    pub(crate) fn pinned_launch_directory(
        &self,
    ) -> Result<
        (
            crate::agent_execution::DirectoryIdentity,
            std::sync::Arc<std::fs::File>,
        ),
        String,
    > {
        match &self.0 {
            Owner::Local(local) => local.pinned_launch_directory(),
            Owner::Remote(remote) => remote.pinned_launch_directory(),
        }
    }
    pub(crate) fn provider_process(&self) -> Option<crate::daemon::holder_registry::Process> {
        match &self.0 {
            Owner::Local(local) => local.provider_process(),
            Owner::Remote(remote) => remote.state().provider_process.clone(),
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
    pub(crate) fn commit_retained_outcomes_until(
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
    /// Identity alone survives epoch changes; grants retain this exact proxy.
    pub(crate) fn same_attachment(&self, other: &Self) -> bool {
        match (&self.0, &other.0) {
            (Owner::Local(a), Owner::Local(b)) => a.same_actor(b),
            (Owner::Remote(a), Owner::Remote(b)) => a.same_attachment(b),
            _ => false,
        }
    }
    pub(crate) fn resolve_retained_source(
        &self,
        source: &chat::Session,
    ) -> Result<chat::Session, String> {
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
    pub(crate) fn try_idle_prompt(&self, text: String) -> Result<(), Rejection> {
        match &self.0 {
            Owner::Local(local) => local.try_idle_prompt(text),
            Owner::Remote(remote) => remote.try_idle_prompt(text),
        }
    }
    /// Exact source recognition is checked by the helper under this attachment epoch.
    pub(crate) fn recognize_retained_sources(
        &self,
        sources: &[chat::Session],
    ) -> Result<(), String> {
        match &self.0 {
            Owner::Remote(remote) => remote.call("recognize_sources", serde_json::json!([sources])),
            Owner::Local(_) => Err("This actor has no retained source authority".into()),
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
    pub(crate) fn close_if_idle(&self, generation: u64) -> Result<(), String> {
        match &self.0 {
            Owner::Local(local) => local.try_close_if_idle(generation).and_then(|closed| {
                if closed {
                    Ok(())
                } else {
                    Err("Provider state changed before conditional idle close".into())
                }
            }),
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
    /// A coherent cached observation, not a permission grant or a reservation
    /// against later reset. The caller must recheck this exact attachment and
    /// current source/permission at final delivery admission.
    pub(crate) fn peer_page(
        &self,
        reader: &mut super::PeerReader,
        after: Option<super::PeerCursor>,
    ) -> Result<super::PeerPage, String> {
        match &self.0 {
            Owner::Local(local) => local.peer_page(reader, after),
            Owner::Remote(remote) => remote.peer_page(reader, after),
        }
    }
    /// Copy is served by the same Store which published the immutable source.
    pub(crate) fn text_copy(
        &self,
        source: &chat::TextCopySource,
        offset: u64,
    ) -> Result<chat::TextCopyPage, String> {
        match &self.0 {
            Owner::Local(local) => local.text_copy(source, offset),
            Owner::Remote(remote) => remote.call("text_copy", serde_json::json!([source, offset])),
        }
    }
    pub(crate) fn history_is_covered_by(&self, reloaded: &Store) -> Result<bool, String> {
        match &self.0 {
            Owner::Local(local) => local.history_is_covered_by(reloaded),
            Owner::Remote(_) => Err(
                "Retained provider ownership requires authenticated close before release".into(),
            ),
        }
    }
    pub(crate) fn is_read_only_history(&self) -> bool {
        matches!(&self.0, Owner::Local(local) if local.is_read_only_history())
    }
    pub(crate) fn resident_generation(&self) -> u64 {
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
    pub(crate) fn try_close_if_idle(&self, generation: u64) -> Result<bool, String> {
        match &self.0 {
            Owner::Local(local) => local.try_close_if_idle(generation),
            Owner::Remote(remote) => remote.try_close_if_idle(generation),
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
    pub(crate) fn continuity_loss(&self) -> Option<super::retained_actor::journal::ContinuityLoss> {
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
    ) -> Result<Option<crate::terminal_ipc::ServerResponse>, String> {
        match &self.0 {
            Owner::Local(local) => Ok(local.presentation(chat_id, after)),
            Owner::Remote(remote) => remote.presentation(chat_id, after),
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
    pub fn close(&self) -> Result<(), String> {
        match &self.0 {
            Owner::Local(local) => local.close(),
            Owner::Remote(remote) => remote.close(),
        }
    }
    pub(crate) fn decline_answer(&self, command: Command) -> Result<(), String> {
        match &self.0 {
            Owner::Local(local) => local.decline_answer(command),
            Owner::Remote(remote) => remote.decline_answer(command),
        }
    }
    pub(crate) fn notification_content(
        &self,
        kind: crate::notifications::Kind,
    ) -> crate::notifications::Content {
        match &self.0 {
            Owner::Local(local) => local.notification_content(kind),
            Owner::Remote(remote) => remote.notification_content(kind),
        }
    }
    pub(crate) fn conversation_title(
        &self,
    ) -> Result<(Option<String>, Option<String>), TitleUnavailable> {
        match &self.0 {
            Owner::Local(local) => local.conversation_title(),
            Owner::Remote(remote) => remote.conversation_title(),
        }
    }
    #[cfg(test)]
    pub(crate) fn fixture_retained_native_holder(
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
