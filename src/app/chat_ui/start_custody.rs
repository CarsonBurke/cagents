use super::*;
use crate::app::channel_ui::SessionCloseContext;
use crate::chat::start_custody::StartCustody;

const CLEANUP_RETRY: &str = "The previous conversation start could not be confirmed stopped. Use Close on its pane to retry cleanup before sending, moving or removing it.";

#[derive(Clone, Copy)]
pub(super) enum StartCleanupScope {
    Project(crate::model::ProjectId),
    Channel(crate::model::ChannelId),
}

pub(super) struct StartAdmission {
    workspace: crate::model::WorkspaceId,
    writer: Option<Arc<crate::persistence::StateStore>>,
    runtime: Target,
    valid: bool,
}

/// A private pane-lifetime fence, never provider retirement authority.
pub(super) struct StartPaneOwner {
    target: Target,
    handle: Arc<ClientHandle>,
}

type Checkpoint = Result<std::sync::mpsc::Receiver<Result<(), String>>, String>;

pub(super) fn await_checkpoint(checkpoint: Checkpoint) -> Result<(), String> {
    checkpoint?
        .recv_timeout(Duration::from_secs(30))
        .map_err(|error| format!("Conversation cleanup checkpoint: {error}"))?
}

/// Read only: never stamps the original request or starts an owner.
fn resolve(custody: &StartCustody, context: &SessionCloseContext) -> Result<chat::Session, String> {
    let mut client = context.custody_connection(custody)?;
    let owners = client.list_chats().map_err(|error| error.to_string())?;
    let candidates: Vec<_> = owners
        .into_iter()
        .filter(|owner| owner.id == custody.source.id)
        .collect();
    if let Some(observed) = &custody.observed {
        if !custody.matches_observed(observed)
            || candidates
                .iter()
                .any(|owner| !custody.permits_observed_identity_learning(observed, owner))
        {
            return Err("The conversation owner changed; the replacement was left open".into());
        }
        return Ok(observed.clone());
    }
    let [owner] = candidates.as_slice() else {
        return Err("The original start has no exact authenticated owner in the daemon inventory. Cleanup remains protected.".into());
    };
    if !custody.matches_observed(owner) {
        return Err(
            "The observed conversation does not match the original start; it was left open".into(),
        );
    }
    Ok(owner.clone())
}

/// Called only after the exact observed descriptor was durably checkpointed.
fn close(custody: &StartCustody, context: &SessionCloseContext) -> Result<(), String> {
    let observed = custody
        .observed
        .as_ref()
        .filter(|owner| custody.matches_observed(owner))
        .ok_or("The observed start owner has not been checkpointed")?;
    let mut client = context.custody_connection(custody)?;
    let owners = client.list_chats().map_err(|error| error.to_string())?;
    if owners.iter().any(|owner| {
        owner.id == observed.id && !custody.permits_observed_identity_learning(observed, owner)
    }) {
        return Err("The conversation owner changed; the replacement was left open".into());
    }
    if owners.iter().any(|owner| owner == observed) && client.close_chat(observed.clone()).is_ok() {
        return Ok(());
    }
    // No absence inference: an absent owner still needs the existing exact
    // authenticated retained-close outcome, including a verified old receipt.
    let mut client = context.custody_connection(custody)?;
    client
        .close_retained_chat(observed.clone())
        .map_err(|error| error.to_string())
}

impl Cagents {
    pub(in crate::app) fn retry_project_start_custody_operation(
        &mut self,
        project: crate::model::ProjectId,
        operation: uuid::Uuid,
    ) -> Task<Message> {
        if !self.unresolved_start_custody().any(|entry| {
            entry.operation == operation && self.project_owns_start_custody(project, entry)
        }) {
            return Task::none();
        }
        self.chat_ui
            .custody_cleanup_scopes
            .insert(operation, StartCleanupScope::Project(project));
        self.retry_start_custody_operation(operation, false)
    }

    pub(in crate::app) fn retry_channel_start_custody_operation(
        &mut self,
        channel: crate::model::ChannelId,
        operation: uuid::Uuid,
    ) -> Task<Message> {
        if !self.unresolved_start_custody().any(|entry| {
            entry.operation == operation && self.channel_owns_start_custody(channel, entry)
        }) {
            return Task::none();
        }
        self.chat_ui
            .custody_cleanup_scopes
            .insert(operation, StartCleanupScope::Channel(channel));
        self.retry_start_custody_operation(operation, false)
    }

    fn custody_cleanup_scope_current(&self, entry: &StartCustody) -> bool {
        match self.chat_ui.custody_cleanup_scopes.get(&entry.operation) {
            Some(StartCleanupScope::Project(project)) => {
                self.project_owns_start_custody(*project, entry)
            }
            Some(StartCleanupScope::Channel(channel)) => {
                self.channel_owns_start_custody(*channel, entry)
            }
            None => true, // Original failed-start retirement or explicit pane Close.
        }
    }

    pub(in crate::app) fn start_custody_cleanup_pending(&self, operation: uuid::Uuid) -> bool {
        self.chat_ui.custody_cleanup.contains(&operation)
            || self.chat_ui.custody_releases.contains_key(&operation)
            || self
                .chat_ui
                .start_custody
                .iter()
                .any(|(target, (nonce, _))| {
                    *nonce == operation
                        && (self.chat_ui.attempt(target.card).is_some()
                            || self.chat_ui.abandoned.contains_key(target))
                })
    }

    pub(in crate::app) fn chat_has_unresolved_start_custody(&self, card: CardId) -> bool {
        self.durable
            .native_start_custody
            .iter()
            .any(|entry| entry.card == card)
            || self
                .chat_ui
                .custody_releases
                .values()
                .any(|entry| entry.card == card)
    }

    pub(in crate::app) fn unresolved_start_custody(&self) -> impl Iterator<Item = &StartCustody> {
        self.durable
            .native_start_custody
            .iter()
            .chain(self.chat_ui.custody_releases.values())
    }

    pub(in crate::app) fn channel_owns_start_custody(
        &self,
        channel: crate::model::ChannelId,
        entry: &StartCustody,
    ) -> bool {
        if let Some(owner) = entry.channel {
            return owner == channel;
        }
        self.durable.channels.iter().find(|owner| owner.id == channel).is_some_and(|owner| {
            entry.project.is_none_or(|project| project == owner.project_id)
                && owner.cards.contains(&entry.card)
                && self.card_state(entry.card).is_some_and(|card| matches!(&card.kind, CardKind::Chat(current) if current == &entry.source || Some(current) == entry.observed.as_ref()))
        })
    }

    pub(in crate::app) fn channel_start_custody_blocker(
        &self,
        channel: crate::model::ChannelId,
    ) -> Option<String> {
        self.unresolved_start_custody()
            .any(|entry| self.channel_owns_start_custody(channel, entry))
            .then(|| CLEANUP_RETRY.into())
    }

    pub(in crate::app) fn project_start_custody_blocker(
        &self,
        project: crate::model::ProjectId,
    ) -> Option<String> {
        self.unresolved_start_custody()
            .any(|entry| self.project_owns_start_custody(project, entry))
            .then(|| CLEANUP_RETRY.into())
    }

    pub(super) fn custody_checkpoint(&mut self) -> Checkpoint {
        self.snapshot_workspace_scope();
        let saved = self.persistent_workspace();
        let writer = self
            .session
            .as_ref()
            .ok_or("Conversation start requires a working session writer")?;
        // An ordered snapshot cannot coalesce away the exact pre-effect fact.
        writer.snapshot_conversion(saved);
        writer.request_flush()
    }

    pub(super) fn start_custody(&mut self, target: Target, source: &chat::Session) -> uuid::Uuid {
        let context = SessionCloseContext::capture(self.workspace_model.identity);
        let channel = self
            .durable
            .channels
            .iter()
            .find(|channel| channel.cards.contains(&target.card));
        let operation = uuid::Uuid::new_v4();
        let entry = StartCustody {
            operation,
            card: target.card,
            workspace: self.durable.id,
            source: source.clone(),
            project: channel.map(|channel| channel.project_id),
            channel: channel.map(|channel| channel.id),
            directory: self.durable.card_scopes.get(&target.card).cloned(),
            registry: context.custody_registry(),
            device: crate::chat::start_custody::device_key(),
            caller: context.custody_caller(),
            destination: None,
            observed: None,
            failure: None,
        };
        Arc::make_mut(&mut self.durable)
            .native_start_custody
            .push(entry);
        self.chat_ui
            .start_custody
            .insert(target, (operation, context));
        if let Some(CardRuntime::Chat(runtime)) = self.cards.get(&target.card) {
            self.chat_ui.custody_admissions.insert(
                target,
                StartAdmission {
                    workspace: self.durable.id,
                    writer: self.session.as_ref().map(|writer| writer.operation_store()),
                    runtime: runtime.target,
                    valid: runtime.session == *source,
                },
            );
        }

        operation
    }

    fn exact_start_custody(&self, target: Target, operation: uuid::Uuid) -> Option<&StartCustody> {
        let (current, _) = self.chat_ui.start_custody.get(&target)?;
        let admission = self.chat_ui.custody_admissions.get(&target)?;
        let writer = self.session.as_ref()?.operation_store();
        if !admission.valid
            || admission.workspace != self.durable.id
            || !admission
                .writer
                .as_ref()
                .is_some_and(|original| Arc::ptr_eq(original, &writer))
        {
            return None;
        }
        let attempt = self.chat_ui.attempt(target.card)?;
        self.durable.native_start_custody.iter().find(|entry| {
            *current == operation && entry.operation == operation && entry.card == target.card
                && entry.workspace == admission.workspace
                && attempt.target == target && attempt.session == entry.source
                && self.card_state(target.card).is_some_and(|card| card.existence.is_open() && matches!(&card.kind, CardKind::Chat(source) if source == &entry.source))
                && matches!(self.cards.get(&target.card), Some(CardRuntime::Chat(runtime)) if runtime.target == admission.runtime && runtime.session == entry.source)
        })
    }

    /// Descriptor replacement invalidates the old prewrite admission even if
    /// another update restores the same bytes before its checkpoint returns.
    pub(in crate::app) fn invalidate_start_custody_source(&mut self, card: CardId) {
        for (target, admission) in &mut self.chat_ui.custody_admissions {
            if target.card == card {
                admission.valid = false;
            }
        }
    }

    pub(super) fn prepare_start_write(
        &mut self,
        target: Target,
        operation: uuid::Uuid,
        proof: HostSessionProof,
        response: chat::client::StartCheckpoint,
    ) -> Task<Message> {
        let Some(entry) = self.exact_start_custody(target, operation).cloned() else {
            response.answer(Err(
                "The conversation start was cancelled or replaced".into()
            ));
            return Task::none();
        };
        let context = &self.chat_ui.start_custody[&target].1;
        if context.scoped()
            || !proof.is_complete()
            || context.admit_custody(&entry).is_err()
            || self.workspace_model.identity
                != entry
                    .registry
                    .unwrap_or(crate::workspace_model::NO_REGISTRY)
            || entry.destination.as_ref().is_some_and(|old| old != &proof)
        {
            response.answer(Err(
                "The original start context changed before dispatch".into()
            ));
            return Task::none();
        }
        if let Some(entry) = Arc::make_mut(&mut self.durable)
            .native_start_custody
            .iter_mut()
            .find(|entry| entry.operation == operation)
        {
            entry.destination = Some(proof.clone());
        }
        let checkpoint = self.custody_checkpoint();
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || await_checkpoint(checkpoint))
                .await
                .map_err(|error| error.to_string())
                .and_then(|result| result);
            Message::ChatUi(Action::StartCheckpointed(
                target, operation, proof, response, result,
            ))
        })
    }

    pub(super) fn start_checkpointed(
        &mut self,
        target: Target,
        operation: uuid::Uuid,
        proof: HostSessionProof,
        response: chat::client::StartCheckpoint,
        result: Result<(), String>,
    ) -> Task<Message> {
        let admitted = self
            .exact_start_custody(target, operation)
            .is_some_and(|entry| {
                entry.destination.as_ref() == Some(&proof)
                    && self.workspace_model.identity
                        == entry
                            .registry
                            .unwrap_or(crate::workspace_model::NO_REGISTRY)
                    && !self.chat_ui.start_custody[&target].1.scoped()
                    && self.chat_ui.start_custody[&target]
                        .1
                        .admit_custody(entry)
                        .is_ok()
            });
        response.answer(result.and_then(|()| {
            admitted
                .then_some(())
                .ok_or("The conversation start was cancelled or its context changed".into())
        }));
        Task::none()
    }

    /// An explicit retry uses the original captured capability while alive,
    /// or the exact matching caller role after restart. It never falls back.
    pub(in crate::app) fn retry_start_custody(
        &mut self,
        card: CardId,
        close_pane: bool,
    ) -> Task<Message> {
        let Some(operation) = self
            .durable
            .native_start_custody
            .iter()
            .find(|entry| entry.card == card)
            .map(|entry| entry.operation)
        else {
            if let Some(entry) = self
                .chat_ui
                .custody_releases
                .values()
                .find(|entry| entry.card == card)
            {
                self.chat_ui
                    .custody_cleanup_requested
                    .insert(entry.operation);
                if close_pane {
                    self.chat_ui.custody_close_requested.insert(entry.operation);
                }
                self.status = Some(TransientNotice::warning(
                    "Closing this conversation after its save finishes; your draft stays until cleanup is confirmed.",
                ));
            }
            return Task::none();
        };
        self.retry_start_custody_operation(operation, close_pane)
    }

    pub(in crate::app) fn retry_start_custody_operation(
        &mut self,
        operation: uuid::Uuid,
        close_pane: bool,
    ) -> Task<Message> {
        let Some(entry) = self
            .durable
            .native_start_custody
            .iter()
            .find(|entry| entry.operation == operation)
            .cloned()
        else {
            return Task::none();
        };
        self.chat_ui
            .custody_cleanup_requested
            .insert(entry.operation);
        if close_pane {
            self.chat_ui.custody_close_requested.insert(entry.operation);
        }
        if self.chat_ui.custody_cleanup.contains(&entry.operation) {
            return Task::none();
        }
        // A start still waiting for Ready may write after its checkpoint. Its
        // exact Ready settles that attempt before cleanup is allowed to begin.
        if let Some(target) =
            self.chat_ui
                .start_custody
                .iter()
                .find_map(|(target, (operation, _))| {
                    (*operation == entry.operation
                        && self
                            .chat_ui
                            .attempt(target.card)
                            .is_some_and(|attempt| attempt.target == *target))
                    .then_some(*target)
                })
        {
            // Close/explicit cleanup may still beat the same-socket write
            // checkpoint. Permanently revoke that admission; after a physical
            // write the client still reports uncertainty and needs cleanup.
            if let Some(admission) = self.chat_ui.custody_admissions.get_mut(&target) {
                admission.valid = false;
            }
            let cancelled = self.cancel_queued(target);
            self.status = Some(TransientNotice::warning(
                "Closing this conversation after its pending start settles; your draft stays until cleanup is confirmed.",
            ));
            return cancelled;
        }
        if self
            .chat_ui
            .start_custody
            .iter()
            .any(|(target, (nonce, _))| {
                *nonce == entry.operation && self.chat_ui.abandoned.contains_key(target)
            })
        {
            if close_pane {
                self.chat_ui.custody_close_requested.insert(entry.operation);
            }
            self.status = Some(TransientNotice::warning(
                "The cancelled conversation start is settling. Its pane stays until cleanup is confirmed.",
            ));
            return Task::none();
        }
        if entry.destination.is_none() {
            // Without a durable same-socket destination checkpoint this gate
            // never permitted ChatStart. This is local record cleanup only.
            return self.release_start_custody(entry.operation, close_pane, true);
        }
        let context = self
            .chat_ui
            .start_custody
            .values()
            .find(|(operation, _)| *operation == entry.operation)
            .map(|(_, context)| context.clone())
            .unwrap_or_else(|| SessionCloseContext::capture(self.workspace_model.identity));
        if let Err(error) = context.admit_custody(&entry) {
            self.status = Some(TransientNotice::warning(error));
            return Task::none();
        }
        if entry.workspace != self.durable.id {
            return Task::none();
        }
        let Some(writer) = self.session.as_ref().map(|writer| writer.operation_store()) else {
            self.status = Some(TransientNotice::warning(
                "Restore saving before retrying conversation cleanup.",
            ));
            return Task::none();
        };
        self.chat_ui
            .custody_cleanup_writers
            .insert(entry.operation, writer);
        self.chat_ui.custody_cleanup.insert(entry.operation);
        let checkpoint = self.custody_checkpoint();
        cosmic::task::future(async move {
            let operation = entry.operation;
            let result = tokio::task::spawn_blocking(move || {
                await_checkpoint(checkpoint)?;
                if entry.destination.is_none() {
                    return Ok(None);
                }
                resolve(&entry, &context).map(Some)
            })
            .await
            .map_err(|error| error.to_string())
            .and_then(|result| result);
            Message::ChatUi(Action::StartOwnerResolved(operation, close_pane, result))
        })
    }

    pub(super) fn start_owner_resolved(
        &mut self,
        operation: uuid::Uuid,
        close_pane: bool,
        result: Result<Option<chat::Session>, String>,
    ) -> Task<Message> {
        let Some(entry) = self
            .durable
            .native_start_custody
            .iter()
            .find(|entry| entry.operation == operation)
            .cloned()
        else {
            return Task::none();
        };
        if !self.chat_ui.custody_cleanup.contains(&operation) {
            return Task::none();
        }
        if !self.custody_cleanup_writer_current(&entry)
            || !self.custody_cleanup_scope_current(&entry)
        {
            return self.finish_start_cleanup(operation, close_pane, Err("The window or session writer changed before cleanup. Return to the original workspace and retry.".into()));
        }
        let observed = match result {
            Err(error) => return self.finish_start_cleanup(operation, close_pane, Err(error)),
            Ok(None) if entry.destination.is_none() => {
                return self.finish_start_cleanup(operation, close_pane, Ok(()));
            }
            Ok(Some(owner)) if entry.matches_observed(&owner) => owner,
            _ => {
                return self.finish_start_cleanup(
                    operation,
                    close_pane,
                    Err("The original conversation owner changed".into()),
                );
            }
        };
        let context = self
            .chat_ui
            .start_custody
            .values()
            .find(|(nonce, _)| *nonce == operation)
            .map(|(_, context)| context.clone())
            .unwrap_or_else(|| SessionCloseContext::capture(self.workspace_model.identity));
        if let Err(error) = context.admit_custody(&entry) {
            return self.finish_start_cleanup(operation, close_pane, Err(error));
        }
        let entry = Arc::make_mut(&mut self.durable)
            .native_start_custody
            .iter_mut()
            .find(|entry| entry.operation == operation)
            .unwrap();
        entry.observed = Some(observed);
        let saved = entry.clone();
        let checkpoint = self.custody_checkpoint();
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || await_checkpoint(checkpoint))
                .await
                .map_err(|error| error.to_string())
                .and_then(|result| result);
            Message::ChatUi(Action::StartOwnerCheckpointed(
                saved, context, close_pane, result,
            ))
        })
    }

    pub(super) fn start_owner_checkpointed(
        &mut self,
        entry: StartCustody,
        context: SessionCloseContext,
        close_pane: bool,
        result: Result<(), String>,
    ) -> Task<Message> {
        if !self.chat_ui.custody_cleanup.contains(&entry.operation)
            || !self
                .durable
                .native_start_custody
                .iter()
                .any(|current| current == &entry)
        {
            return Task::none();
        }
        if !self.custody_cleanup_writer_current(&entry)
            || !self.custody_cleanup_scope_current(&entry)
        {
            return self.finish_start_cleanup(
                entry.operation,
                close_pane,
                Err(
                    "The window or session writer changed before Close; no replacement was closed."
                        .into(),
                ),
            );
        }
        if let Err(error) = result.and_then(|()| context.admit_custody(&entry)) {
            return self.finish_start_cleanup(entry.operation, close_pane, Err(error));
        }
        // Pin the actual mounted connection before Close. Its provider may
        // learn a UUID while the exact old alias is being retired. A new
        // connection or descriptor-only replacement cannot inherit this pin.
        if let Some(CardRuntime::Chat(runtime)) = self.cards.get(&entry.card)
            && let Some(handle) = runtime.handle()
            && let Some(current) = self.chat_return_session(entry.card)
            && self.mounted_custody_source_current(&entry, runtime, &current)
        {
            self.chat_ui.custody_pane_owners.insert(
                entry.operation,
                StartPaneOwner {
                    target: runtime.target,
                    handle: handle.clone(),
                },
            );
        }
        cosmic::task::future(async move {
            let operation = entry.operation;
            let result = tokio::task::spawn_blocking(move || close(&entry, &context))
                .await
                .map_err(|error| error.to_string())
                .and_then(|result| result);
            Message::ChatUi(Action::StartCleanupFinished(operation, close_pane, result))
        })
    }

    fn mounted_custody_source_current(
        &self,
        entry: &StartCustody,
        runtime: &Runtime,
        current: &chat::Session,
    ) -> bool {
        // The reader can learn an ID before its Wake reaches the UI. The
        // durable descriptor must still agree exactly with the mounted cache;
        // only that same connection's monotone metadata may be newer.
        self.card_state(entry.card).is_some_and(
            |card| matches!(&card.kind, CardKind::Chat(source) if source == &runtime.session),
        ) && entry.observed.as_ref().is_some_and(|observed| {
            entry.permits_observed_identity_learning(observed, &runtime.session)
                && entry.permits_observed_identity_learning(observed, current)
                && entry.permits_observed_identity_learning(&runtime.session, current)
        })
    }

    fn custody_cleanup_writer_current(&self, entry: &StartCustody) -> bool {
        entry.workspace == self.durable.id
            && self
                .chat_ui
                .custody_cleanup_writers
                .get(&entry.operation)
                .zip(
                    self.session
                        .as_ref()
                        .map(|writer| writer.operation_store())
                        .as_ref(),
                )
                .is_some_and(|(original, current)| Arc::ptr_eq(original, current))
            && self.workspace_model.identity
                == entry
                    .registry
                    .unwrap_or(crate::workspace_model::NO_REGISTRY)
    }

    pub(super) fn finish_start_cleanup(
        &mut self,
        operation: uuid::Uuid,
        close_pane: bool,
        result: Result<(), String>,
    ) -> Task<Message> {
        self.chat_ui.custody_cleanup_writers.remove(&operation);
        if !self.chat_ui.custody_cleanup.remove(&operation) {
            return Task::none();
        }
        let Some(entry) = self
            .durable
            .native_start_custody
            .iter()
            .find(|entry| entry.operation == operation)
            .cloned()
        else {
            return Task::none();
        };
        match result {
            Ok(()) => return self.release_start_custody(operation, close_pane, true),
            Err(error) => {
                self.chat_ui.custody_pane_owners.remove(&operation);
                if let Some(entry) = Arc::make_mut(&mut self.durable)
                    .native_start_custody
                    .iter_mut()
                    .find(|entry| entry.operation == operation)
                {
                    entry.failure = Some(error.clone());
                }
                self.persist();
                self.status = Some(TransientNotice::warning(format!("{CLEANUP_RETRY} {error}")));
            }
        }
        Task::none()
    }
    pub(super) fn release_start_custody(
        &mut self,
        operation: uuid::Uuid,
        close_pane: bool,
        ended: bool,
    ) -> Task<Message> {
        let Some(entry) = self
            .durable
            .native_start_custody
            .iter()
            .find(|entry| entry.operation == operation)
            .cloned()
        else {
            return Task::none();
        };
        if self.chat_ui.custody_releases.contains_key(&operation) {
            return Task::none();
        }
        if entry.workspace != self.durable.id {
            self.status = Some(TransientNotice::warning(
                "Return to the original workspace before finishing conversation cleanup.",
            ));
            return Task::none();
        }
        if let Some(writer) = self.session.as_ref() {
            self.chat_ui
                .custody_release_writers
                .insert(operation, writer.operation_store());
        }
        let durable = Arc::make_mut(&mut self.durable);
        durable
            .native_start_custody
            .retain(|entry| entry.operation != operation);
        durable.released_native_starts.insert(operation);
        self.chat_ui
            .custody_releases
            .insert(operation, entry.clone());
        let checkpoint = self.custody_checkpoint();
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || await_checkpoint(checkpoint))
                .await
                .map_err(|error| error.to_string())
                .and_then(|result| result);
            Message::ChatUi(Action::StartCustodyReleased(
                entry, close_pane, ended, result,
            ))
        })
    }

    pub(super) fn start_custody_released(
        &mut self,
        entry: StartCustody,
        close_pane: bool,
        ended: bool,
        result: Result<(), String>,
    ) -> Task<Message> {
        if self.chat_ui.custody_releases.get(&entry.operation) != Some(&entry) {
            return Task::none();
        }
        let same_writer = self
            .chat_ui
            .custody_release_writers
            .remove(&entry.operation)
            .zip(self.session.as_ref().map(|writer| writer.operation_store()))
            .is_some_and(|(original, current)| Arc::ptr_eq(&original, &current));
        let result = result.and_then(|()| {
            same_writer.then_some(()).ok_or(
                "The session writer changed before the conversation save was acknowledged".into(),
            )
        });
        let close_pane = close_pane
            || self
                .chat_ui
                .custody_close_requested
                .contains(&entry.operation);
        self.chat_ui.custody_releases.remove(&entry.operation);
        let pane_owner = self.chat_ui.custody_pane_owners.remove(&entry.operation);
        if entry.workspace != self.durable.id {
            self.status = Some(TransientNotice::warning(
                "Return to the original workspace to finish conversation cleanup.",
            ));
            if let Err(error) = result {
                let mut failed = entry;
                failed.failure = Some(error);
                self.chat_ui
                    .custody_releases
                    .insert(failed.operation, failed);
            }
            return Task::none();
        }
        match result {
            Ok(()) => {
                if self
                    .chat_ui
                    .custody_cleanup_requested
                    .contains(&entry.operation)
                    && !ended
                {
                    // Ready saved an authenticated owner, not a Close receipt.
                    // A Close arriving during that ACK gets a new cleanup-only
                    // nonce; the released start operation cannot be resurrected
                    // by stale writers or a late callback.
                    let original = self
                        .chat_ui
                        .start_custody
                        .iter()
                        .find(|(_, (nonce, _))| *nonce == entry.operation)
                        .map(|(target, (_, context))| (*target, context.clone()));
                    self.chat_ui
                        .start_custody
                        .retain(|_, (nonce, _)| *nonce != entry.operation);
                    self.chat_ui
                        .custody_close_requested
                        .remove(&entry.operation);
                    let mut cleanup = entry.clone();
                    cleanup.operation = uuid::Uuid::new_v4();
                    cleanup.failure = None;
                    let operation = cleanup.operation;
                    if let Some(scope) =
                        self.chat_ui.custody_cleanup_scopes.remove(&entry.operation)
                    {
                        self.chat_ui.custody_cleanup_scopes.insert(operation, scope);
                    }
                    Arc::make_mut(&mut self.durable)
                        .native_start_custody
                        .push(cleanup);
                    if let Some((target, context)) = original {
                        self.chat_ui
                            .start_custody
                            .insert(target, (operation, context));
                    }
                    self.chat_ui
                        .custody_cleanup_requested
                        .remove(&entry.operation);
                    self.chat_ui.custody_cleanup_requested.insert(operation);
                    if close_pane {
                        self.chat_ui.custody_close_requested.insert(operation);
                    }
                    return self.retry_start_custody_operation(operation, close_pane);
                }
                self.chat_ui
                    .start_custody
                    .retain(|_, (nonce, _)| *nonce != entry.operation);
                self.chat_ui
                    .custody_close_requested
                    .remove(&entry.operation);
                self.chat_ui
                    .custody_cleanup_requested
                    .remove(&entry.operation);
                self.chat_ui.custody_cleanup_scopes.remove(&entry.operation);
                let active = self
                    .chat_ui
                    .start_custody
                    .keys()
                    .copied()
                    .collect::<HashSet<_>>();
                self.chat_ui
                    .custody_admissions
                    .retain(|target, _| active.contains(target));
                let current = self.chat_return_session(entry.card);
                let pane_matches = current.as_ref().is_some_and(|current| {
                    if let Some(owner) = &pane_owner {
                        matches!(self.cards.get(&entry.card), Some(CardRuntime::Chat(runtime)) if runtime.target == owner.target && runtime.handle().is_some_and(|handle| Arc::ptr_eq(handle, &owner.handle)) && self.mounted_custody_source_current(&entry, runtime, current))
                    } else {
                        self.card_state(entry.card).is_some_and(|card| matches!(&card.kind, CardKind::Chat(source) if source == current))
                            && (current == &entry.source || Some(current) == entry.observed.as_ref())
                    }
                });
                if ended
                    && !close_pane
                    && entry.source.is_unstarted()
                    && pane_matches
                    && current.as_ref() != Some(&entry.source)
                    && !self.pane_transfer_pending(entry.card)
                {
                    self.chat_forget(entry.card);
                    self.cards.remove(&entry.card);
                    let mut draft = entry.source.clone();
                    draft.id = TerminalId::new();
                    self.replace_chat_conversation(entry.card, draft.clone());
                    return self.open_chat_draft(entry.card, draft);
                }
                if close_pane && pane_matches {
                    if self.chat_has_unresolved_start_custody(entry.card) {
                        return self.retry_start_custody(entry.card, true);
                    }
                    if self.pane_transfer_pending(entry.card) {
                        self.status = Some(TransientNotice::warning(
                            "The conversation stopped, but its pane is moving. Finish the move before closing it.",
                        ));
                        return Task::none();
                    }
                    return self
                        .remove_chat_card(entry.card, None)
                        .unwrap_or_else(Task::none);
                }
                if close_pane || (ended && !pane_matches) {
                    self.status = Some(TransientNotice::warning(
                        "The previous conversation stopped. This pane changed while cleanup was pending, so its current conversation was kept.",
                    ));
                }
            }
            Err(error) => {
                let original = self
                    .chat_ui
                    .start_custody
                    .iter()
                    .find(|(_, (nonce, _))| *nonce == entry.operation)
                    .map(|(target, (_, context))| (*target, context.clone()));
                self.chat_ui
                    .start_custody
                    .retain(|_, (nonce, _)| *nonce != entry.operation);
                self.chat_ui
                    .custody_cleanup_requested
                    .remove(&entry.operation);
                self.chat_ui
                    .custody_close_requested
                    .remove(&entry.operation);
                let mut retry = entry.clone();
                retry.operation = uuid::Uuid::new_v4();
                retry.failure = Some(format!("The conversation result was not saved: {error}"));
                let operation = retry.operation;
                if let Some(scope) = self.chat_ui.custody_cleanup_scopes.remove(&entry.operation) {
                    self.chat_ui.custody_cleanup_scopes.insert(operation, scope);
                }
                let durable = Arc::make_mut(&mut self.durable);
                durable.released_native_starts.insert(entry.operation);
                durable.native_start_custody.push(retry);
                if let Some((target, context)) = original {
                    self.chat_ui
                        .start_custody
                        .insert(target, (operation, context));
                }
                if close_pane {
                    self.chat_ui.custody_close_requested.insert(operation);
                }
                self.status = Some(TransientNotice::warning(
                    "The conversation result could not be saved. Keep the pane open and retry Close after saving recovers.",
                ));
            }
        }
        Task::none()
    }
}
