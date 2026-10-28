//! Mirrors the daemon's shared sidebar model into this window's own store.
//!
//! The daemon owns which projects and channels exist; this window owns what is
//! open inside them. Two threads keep the two in step: one watches the registry
//! for edges, one publishes this window's mutations. Neither ever runs on the UI
//! thread, because a redraw must not wait on a daemon that may not be running.

use super::*;
use crate::control::{ControlClient, ControlError};
use crate::model::{ChannelId, ProjectChannel};
use crate::workspace_model::{
    ModelChannel, ModelEdit, ModelProject, NO_REGISTRY, PENDING_ORDINAL, WorkspaceModel,
    canonical_repository,
};

/// How long a thread waits before reconnecting.
///
/// A window can open before any daemon exists, and the daemon can be replaced
/// under it. Neither is an error, so the threads retry rather than giving up -
/// and slowly, because a reconnection re-reads the whole model and therefore
/// cannot miss anything by being late.
const RECONNECT_DELAY: Duration = Duration::from_secs(2);

/// What the daemon threads hand the window.
#[derive(Debug, Clone)]
pub enum ModelUpdate {
    /// The registry as it stood when the watcher connected.
    ///
    /// This is the only update that means "a connection just began", which is
    /// why it is distinct from [`ModelUpdate::Read`]: it is where the window
    /// compares the registry's identity against the one it recorded and decides
    /// whether anything it holds has ever been shared with this registry.
    Connected(WorkspaceModel),
    /// A fresh read, taken because an edge does not carry the ranks it changed.
    Read(WorkspaceModel),
    Reopened {
        repository: PathBuf,
        model: WorkspaceModel,
    },
    ReopenFailed {
        repository: PathBuf,
        error: String,
    },
    Edited {
        sequence: u64,
        edit: ModelEdit,
    },
    /// The rank the registry gave a channel this window created.
    ///
    /// It is also the proof that the registry holds the channel, which is what
    /// marks it shared: from here on, the registry's silence about that channel
    /// means it was deleted somewhere rather than never known.
    ViewApplied {
        scope: crate::workspace_model::views::Scope,
        operation: Option<uuid::Uuid>,
        model: WorkspaceModel,
    },
    NavigationFailed(String),
    /// Optional naming yielded after temporary admission pressure. The next
    /// real wake may retry this exact current candidate; this is not a timer.
    TitleDeferred(ModelEdit),
    PaneTransferFailed {
        operation: uuid::Uuid,
        error: String,
    },
    ViewRefused {
        scope: crate::workspace_model::views::Scope,
        operation: uuid::Uuid,
        error: String,
    },
    /// The host did not apply any part of this edit. Its source basis changed;
    /// ordinary revision retries cannot make that captured intent authoritative.
    ViewConflicted {
        scope: crate::workspace_model::views::Scope,
        operation: uuid::Uuid,
        change: crate::workspace_model::views::Change,
        model: WorkspaceModel,
        error: String,
    },
    Ranked {
        channel: ChannelId,
        ordinal: u32,
    },
}

/// Watches the daemon for shared-model edges.
pub(super) fn observe_workspace_model(sender: async_channel::Sender<ModelUpdate>) {
    let observer = std::thread::Builder::new()
        .name("cagents-workspace-model".into())
        .spawn(move || {
            while !sender.is_closed() {
                if let Err(error) = observe(&sender) {
                    tracing::debug!(%error, "shared workspace model observation ended");
                }
                std::thread::sleep(RECONNECT_DELAY);
            }
        });
    if let Err(error) = observer {
        tracing::warn!(
            %error,
            "could not start the shared workspace model observer; channels created elsewhere \
             will only appear when this window is restarted"
        );
    }
}

fn observe(sender: &async_channel::Sender<ModelUpdate>) -> Result<(), String> {
    // Never start a daemon merely to watch it: with no daemon there is no shared
    // model to mirror, and the window starts one itself as soon as it publishes.
    let socket = crate::terminal::ipc::daemon_socket_path().map_err(|error| error.to_string())?;
    let mut client = ControlClient::connect(&socket).map_err(|error| error.to_string())?;
    // One request subscribes and reads, so an edit applied between the two
    // cannot fall into a gap - the hazard `watch_terminals` then `list` has to
    // order around with two requests.
    let model = client
        .watch_workspace_model()
        .map_err(|error| error.to_string())?;
    sender
        .send_blocking(ModelUpdate::Connected(model))
        .map_err(|error| error.to_string())?;
    loop {
        let (sequence, edit) = client
            .next_workspace_model_edit()
            .map_err(|error| error.to_string())?;
        // View edits and proof-dependent title suggestions need complete
        // authoritative pane data: the cached view may precede their revision.
        // Navigation also reads its settled registry ordering.
        if matches!(
            edit,
            ModelEdit::SeedView { .. }
                | ModelEdit::ChangeView { .. }
                | ModelEdit::SuggestConversationTitle { .. }
                | ModelEdit::ReorderNavigation { .. }
                | ModelEdit::ResetNavigation { .. }
                | ModelEdit::RetireEmptyChannel { .. }
        ) {
            let model = client
                .read_workspace_model()
                .map_err(|error| error.to_string())?;
            sender
                .send_blocking(ModelUpdate::Read(model))
                .map_err(|error| error.to_string())?;
            continue;
        }
        let ranked = matches!(
            edit,
            ModelEdit::CreateChannel { .. }
                | ModelEdit::AgentsChanged
                | ModelEdit::SeedView { .. }
                | ModelEdit::ChangeView { .. }
        );
        sender
            .send_blocking(ModelUpdate::Edited { sequence, edit })
            .map_err(|error| error.to_string())?;
        if ranked {
            let model = client
                .read_workspace_model()
                .map_err(|error| error.to_string())?;
            sender
                .send_blocking(ModelUpdate::Read(model))
                .map_err(|error| error.to_string())?;
        }
    }
}

/// Publishes this window's shared-model mutations.
pub(super) fn publish_workspace_model(
    updates: async_channel::Sender<ModelUpdate>,
    requests: async_channel::Receiver<ModelEdit>,
) {
    let publisher = std::thread::Builder::new()
        .name("cagents-workspace-model-publisher".into())
        .spawn(move || {
            let mut pending = None;
            while !requests.is_closed() || pending.is_some() {
                if let Err(error) = publish(&updates, &requests, &mut pending) {
                    tracing::debug!(%error, "shared workspace model publishing ended");
                }
                std::thread::sleep(RECONNECT_DELAY);
            }
        });
    if let Err(error) = publisher {
        tracing::warn!(
            %error,
            "could not start the shared workspace model publisher; channels created in this \
             window will not reach any other"
        );
    }
}

fn publish(
    updates: &async_channel::Sender<ModelUpdate>,
    requests: &async_channel::Receiver<ModelEdit>,
    pending: &mut Option<ModelEdit>,
) -> Result<(), String> {
    // Unlike the observer, the publisher starts a daemon: a channel the user
    // just created has to reach the registry, and there is no registry without
    // a daemon to hold it.
    let socket = crate::terminal::ensure_daemon_ready().map_err(|error| error.to_string())?;
    let mut client = ControlClient::connect(&socket).map_err(|error| error.to_string())?;
    loop {
        let request = match pending.take() {
            Some(request) => request,
            None => requests
                .recv_blocking()
                .map_err(|_| "the window stopped publishing".to_owned())?,
        };
        // Retained until it is sent, so a daemon replaced mid-publish costs a
        // reconnection rather than the edit.
        *pending = Some(request.clone());
        match send(&mut client, request.clone(), updates) {
            Ok(()) => *pending = None,
            Err(error) => {
                // A refusal is final: the registry rejected the edit itself, so
                // resending it to a fresh connection would be refused
                // identically and retrying would never stop.
                if matches!(
                    &error,
                    ControlError::Remote {
                        retryable: false,
                        ..
                    }
                ) {
                    *pending = None;
                    if matches!(
                        &request,
                        ModelEdit::ReorderNavigation { .. } | ModelEdit::ResetNavigation { .. }
                    ) {
                        let _ =
                            updates.send_blocking(ModelUpdate::NavigationFailed(error.to_string()));
                    }
                    if matches!(&request, ModelEdit::SuggestConversationTitle { .. })
                        && let Ok(model) = client.read_workspace_model()
                    {
                        // A refused suggestion is not replayed against a newer
                        // owner. Restore the authoritative name after optimism.
                        let _ = updates.send_blocking(ModelUpdate::Read(model));
                    }
                    if let ModelEdit::ChangeView {
                        scope, operation, ..
                    } = &request
                    {
                        let _ = updates.send_blocking(ModelUpdate::ViewRefused {
                            scope: scope.clone(),
                            operation: *operation,
                            error: error.to_string(),
                        });
                    }
                    if let ModelEdit::ReopenProject { repository, .. } = request {
                        let _ = updates.send_blocking(ModelUpdate::ReopenFailed {
                            repository,
                            error: error.to_string(),
                        });
                    }
                }
                return Err(error.to_string());
            }
        }
    }
}

fn send(
    client: &mut ControlClient,
    edit: ModelEdit,
    updates: &async_channel::Sender<ModelUpdate>,
) -> Result<(), ControlError> {
    let reordered = matches!(
        &edit,
        ModelEdit::ReorderNavigation { .. } | ModelEdit::ResetNavigation { .. }
    );
    let created = match &edit {
        ModelEdit::CreateChannel { id, .. } => Some(*id),
        _ => None,
    };
    let reopened = match &edit {
        ModelEdit::ReopenProject { repository, .. } => Some(repository.clone()),
        _ => None,
    };
    let view_edit = match &edit {
        ModelEdit::SeedView { scope, .. } => Some((scope.clone(), None)),
        ModelEdit::ChangeView {
            scope, operation, ..
        } => Some((scope.clone(), Some(*operation))),
        _ => None,
    };
    let mut request = edit;
    let mut conflicts = 0;
    let mut title_busy = 0;
    let model = loop {
        match client.edit_workspace_model(request.clone()) {
            Ok(model) => break model,
            Err(ControlError::Remote {
                code: crate::terminal::ipc::ProtocolErrorCode::QueueFull,
                retryable: true,
                ..
            }) if matches!(&request, ModelEdit::SuggestConversationTitle { .. }) => {
                title_busy += 1;
                if title_busy < 3 {
                    std::thread::sleep(Duration::from_millis(20 * title_busy));
                    continue;
                }
                // Optional metadata must not hold every later model edit at
                // the queue head. Do not renew its proof or reconnect/replay it.
                if let Ok(model) = client.read_workspace_model() {
                    let _ = updates.send_blocking(ModelUpdate::Read(model));
                }
                let _ = updates.send_blocking(ModelUpdate::TitleDeferred(request));
                return Ok(());
            }
            Err(
                error @ ControlError::Remote {
                    code: crate::terminal::ipc::ProtocolErrorCode::ViewConflict,
                    ..
                },
            ) => {
                conflicts += 1;
                if conflicts > 8 {
                    return Err(error);
                }
                std::thread::sleep(Duration::from_millis(20 * conflicts));
                let model = client.read_workspace_model()?;
                if let ModelEdit::ChangeView {
                    scope,
                    operation,
                    change,
                    ..
                } = &request
                    && source_update_conflicts(&model, scope, change)
                {
                    let _ = updates.send_blocking(ModelUpdate::ViewConflicted {
                        scope: scope.clone(),
                        operation: *operation,
                        change: change.clone(),
                        model,
                        error: error.to_string(),
                    });
                    // This is an explicit refusal delivered to the UI, never
                    // a successful ViewApplied receipt or a partial commit.
                    return Ok(());
                }
                if let ModelEdit::ChangeView {
                    scope,
                    expected_revision,
                    ..
                } = &mut request
                {
                    *expected_revision = model
                        .views
                        .iter()
                        .find(|view| view.scope == *scope)
                        .map(|view| view.revision)
                        .unwrap_or(0);
                }
            }
            Err(error) => return Err(error),
        }
    };
    if let Some((scope, operation)) = view_edit {
        let _ = updates.send_blocking(ModelUpdate::ViewApplied {
            scope,
            operation,
            model: model.clone(),
        });
    }
    // The originating client is deliberately not sent its own echo, so this
    // response is how the window that minted the channel learns the rank the
    // registry gave it - and learns that the registry now holds it.
    if let Some(id) = created
        && let Some(channel) = model.channel(id)
    {
        let _ = updates.send_blocking(ModelUpdate::Ranked {
            channel: id,
            ordinal: channel.ordinal,
        });
    }
    if reordered
        || matches!(
            &request,
            ModelEdit::SuggestConversationTitle { .. } | ModelEdit::RetireEmptyChannel { .. }
        )
    {
        let _ = updates.send_blocking(ModelUpdate::Read(model.clone()));
    }
    if let Some(repository) = reopened {
        let _ = updates.send_blocking(ModelUpdate::Reopened { repository, model });
    }
    Ok(())
}

fn source_update_conflicts(
    model: &WorkspaceModel,
    scope: &crate::workspace_model::views::Scope,
    change: &crate::workspace_model::views::Change,
) -> bool {
    use crate::workspace_model::views::Change;
    let Change::Update { before, pane } = change else {
        return false;
    };
    let sensitive = before.kind != pane.kind
        || before.pending_terminal_start != pane.pending_terminal_start
        || before.scope != pane.scope;
    sensitive
        && !model
            .views
            .iter()
            .find(|view| &view.scope == scope)
            .and_then(|view| view.panes.iter().find(|current| current.id == before.id))
            .is_some_and(|current| {
                current.kind == before.kind
                    && current.pending_terminal_start == before.pending_terminal_start
                    && current.scope == before.scope
            })
}

/// What adopting one shared row asked of the window.
enum Adoption {
    /// The window's own store changed.
    Applied,
    /// A repository has to be read before the row can be shown.
    Opening(Task<Message>),
    /// Nothing this window can show yet; the row is retained in the model and
    /// reconsidered on the next reconciliation.
    Retained,
}

impl Cagents {
    pub(super) fn reorder_sidebar_member(
        &mut self,
        project: crate::model::ProjectId,
        source: crate::sidebar::SelectionRow,
        before: Option<crate::sidebar::SelectionRow>,
    ) -> Task<Message> {
        use crate::workspace_model::{ModelEdit, NavigationItem};
        let Some(entity) = self.sidebar_model.project(project) else {
            return Task::none();
        };
        let item = |row| match row {
            crate::sidebar::SelectionRow::Channel(id) => entity
                .channels
                .iter()
                .find(|row| {
                    row.id == id && row.deletion != Some(crate::sidebar::RowDeletion::Deleting)
                })
                .map(|row| NavigationItem::Channel(row.id)),
            crate::sidebar::SelectionRow::Worktree(owner, id) if owner == project => entity
                .worktrees
                .iter()
                .find(|row| {
                    row.id == id && row.deletion != Some(crate::sidebar::RowDeletion::Deleting)
                })
                .map(|row| NavigationItem::Worktree(row.path.clone())),
            _ => None,
        };
        let Some(source) = item(source) else {
            return Task::none();
        };
        let before = match before {
            Some(row) => match item(row) {
                Some(item) => Some(item),
                None => return Task::none(),
            },
            None => None,
        };
        if before.as_ref() == Some(&source) {
            return Task::none();
        }
        let mut fallback: Vec<_> = crate::sidebar::channels_in_display_order(
            entity
                .channels
                .iter()
                .filter(|row| row.deletion != Some(crate::sidebar::RowDeletion::Deleting)),
        )
        .into_iter()
        .map(|row| NavigationItem::Channel(row.id))
        .chain(
            crate::sidebar::worktrees_in_display_order(
                &entity.repository,
                self.settings.worktree_sort,
                entity
                    .worktrees
                    .iter()
                    .filter(|row| row.deletion != Some(crate::sidebar::RowDeletion::Deleting)),
            )
            .into_iter()
            .map(|row| NavigationItem::Worktree(row.path.clone())),
        )
        .collect();
        crate::sidebar::apply_navigation_order(
            &mut fallback,
            &entity.navigation_order,
            Clone::clone,
        );
        let edit = ModelEdit::ReorderNavigation {
            repository: entity.repository.clone(),
            source,
            before,
            fallback,
        };
        self.publish_model_edit(edit);
        Task::none()
    }

    /// Keep complete view/agent snapshots ordered independently of observed
    /// metadata edges. A title/rank/project edge has no replacement pane data.
    pub(super) fn install_workspace_model_snapshot(&mut self, mut model: WorkspaceModel) -> bool {
        if model.identity != self.workspace_model.identity
            || model.sequence >= self.workspace_model.sequence
        {
            self.shared_views.snapshot_sequence = model.sequence;
            self.workspace_model = model;
            return true;
        }
        if model.sequence < self.shared_views.snapshot_sequence {
            return false;
        }
        self.shared_views.snapshot_sequence = model.sequence;
        // Preserve newer folded metadata, including explicit project/channel
        // removal, while accepting the complete, newer collection evidence.
        self.workspace_model.views = std::mem::take(&mut model.views);
        self.workspace_model.agents = std::mem::take(&mut model.agents);
        true
    }

    pub(super) fn apply_workspace_model_update(&mut self, update: ModelUpdate) -> Task<Message> {
        self.apply_workspace_model_update_effect(update).0
    }

    /// Only stale complete snapshots skip projection. Even an equal model can
    /// restore optimistic local state after refusal (for example channel titles).
    /// Newer metadata edges may still lack an older snapshot's full inventory.
    pub(super) fn apply_workspace_model_update_effect(
        &mut self,
        update: ModelUpdate,
    ) -> (Task<Message>, bool) {
        if let ModelUpdate::Read(model) = &update
            && model.identity == self.workspace_model.identity
            && model.sequence < self.shared_views.snapshot_sequence
        {
            return (Task::none(), false);
        }
        (self.apply_workspace_model_update_inner(update), true)
    }

    fn apply_workspace_model_update_inner(&mut self, update: ModelUpdate) -> Task<Message> {
        match update {
            ModelUpdate::TitleDeferred(edit) => {
                self.defer_channel_title_suggestion(&edit);
                Task::none()
            }
            ModelUpdate::ViewConflicted {
                scope,
                operation,
                change,
                model,
                error,
            } => self.refuse_conflicting_view_update(scope, operation, change, model, error),
            ModelUpdate::PaneTransferFailed { operation, error } => {
                self.reject_pane_transfer(operation, error);
                self.reconcile_shared_views()
            }
            ModelUpdate::ViewRefused {
                scope,
                operation,
                error,
            } => {
                tracing::warn!(?scope, %operation, %error, "the registry refused a view edit");
                if self.shared_views.is_requested_transfer(operation) {
                    self.reject_pane_transfer(operation, error);
                } else {
                    self.shared_views.refused(operation);
                    self.status = Some(TransientNotice::warning(format!(
                        "The pane change was not applied: {error}"
                    )));
                }
                self.reconcile_shared_views()
            }
            ModelUpdate::NavigationFailed(error) => {
                self.status = Some(TransientNotice::warning(format!(
                    "Could not reorder the sidebar: {error}"
                )));
                Task::none()
            }
            ModelUpdate::ViewApplied {
                scope,
                operation,
                model,
            } => {
                if self.workspace_model.identity != crate::workspace_model::NO_REGISTRY
                    && model.identity != self.workspace_model.identity
                {
                    return Task::none();
                }
                let transferred = operation
                    .and_then(|operation| self.shared_views.requested_transfer_card(operation));
                self.install_workspace_model_snapshot(model);
                self.acknowledge_view_edit(scope, operation);
                self.retire_settled_empty_channels();
                self.retarget_transferred_pane_updates(transferred);
                self.reconcile_workspace_model()
            }
            ModelUpdate::Connected(model) => {
                if model.identity == self.workspace_model.identity
                    && model.sequence < self.shared_views.snapshot_sequence
                {
                    return Task::none();
                }
                self.join_workspace_model(model.identity);
                self.install_workspace_model_snapshot(model);
                let closed = self.apply_closed_project_tombstones();
                Task::batch([closed, self.reconcile_workspace_model()])
            }
            ModelUpdate::Read(model) => {
                if !self.install_workspace_model_snapshot(model) {
                    return Task::none();
                }
                let closed = self.apply_closed_project_tombstones();
                Task::batch([closed, self.reconcile_workspace_model()])
            }
            ModelUpdate::Reopened { repository, model } => {
                if model.identity == self.workspace_model.identity
                    && model.sequence < self.workspace_model.sequence
                {
                    self.project_ui
                        .reopening
                        .retain(|_, path| path != &repository);
                    return self.apply_closed_project_tombstones();
                }
                self.join_workspace_model(model.identity);
                if model.project(&repository).is_some() {
                    self.cancel_shared_project_close(&repository);
                    self.project_ui.closed.remove(&repository);
                    self.project_ui
                        .reopening
                        .retain(|_, path| path != &repository);
                }
                self.install_workspace_model_snapshot(model);
                let closed = self.apply_closed_project_tombstones();
                Task::batch([closed, self.reconcile_workspace_model()])
            }
            ModelUpdate::ReopenFailed { repository, error } => {
                self.project_ui
                    .reopening
                    .retain(|_, path| path != &repository);
                self.status = Some(crate::app::TransientNotice::error(format!(
                    "Could not reopen project: {error}"
                )));
                self.apply_closed_project_tombstones()
            }
            ModelUpdate::Edited { sequence, edit } => {
                // Work commits and metadata announcements can cross between
                // independent WAL connections. Even an older invalidation
                // must refresh its exact home; it never replays a model edit.
                if let ModelEdit::ChannelWorkChanged { home } = &edit {
                    if home.registry != self.workspace_model.identity {
                        return Task::none();
                    }
                    // This is an independent invalidation, not a folded
                    // canonical edit. Advancing that watermark could discard
                    // a delayed, lower-sequence channel/project edit.
                    return if self.channel_work.view.home() == Some(*home) {
                        self.channel_work
                            .view
                            .refresh()
                            .map_or_else(Task::none, |effect| self.channel_work_effect(effect))
                    } else {
                        Task::none()
                    };
                }

                if sequence <= self.workspace_model.sequence {
                    return Task::none();
                }
                // An explicit delete is an instruction rather than an absence,
                // so folding it in retires the channel whether or not this
                // window had recorded the channel as shared - which is exactly
                // the case absence may not be trusted in.
                let deleted = match &edit {
                    ModelEdit::DeleteChannel { id } => Some(*id),
                    _ => None,
                };
                let closing = match &edit {
                    ModelEdit::CloseProject { repository } => Some(repository.clone()),
                    ModelEdit::ReopenProject { repository, .. } => {
                        self.cancel_shared_project_close(repository);
                        self.project_ui.closed.remove(repository);
                        // The observer can see a reopen from another window
                        // before this window's own publish reply. Only that
                        // reply completes this window's pending intent.
                        None
                    }
                    ModelEdit::OpenProject { .. } => None,
                    _ => None,
                };
                self.workspace_model.fold(sequence, &edit);
                let removed = closing.map_or_else(Task::none, |repository| {
                    self.close_shared_project(&repository)
                });
                let retired = deleted.map_or_else(Task::none, |id| {
                    // The registry has authoritatively removed this identity,
                    // even if its creation acknowledgment never reached us.
                    // A failed session close must not make it publishable again.
                    if let Some(channel) = Arc::make_mut(&mut self.durable)
                        .channels
                        .iter_mut()
                        .find(|channel| channel.id == id)
                    {
                        channel.shared = true;
                    }
                    self.retire_peer_channel(id)
                });
                Task::batch([removed, retired, self.reconcile_workspace_model()])
            }
            ModelUpdate::Ranked { channel, ordinal } => {
                let Some(index) = self
                    .durable
                    .channels
                    .iter()
                    .position(|local| local.id == channel)
                else {
                    return Task::none();
                };
                let local = &self.durable.channels[index];
                if local.ordinal == ordinal && local.shared {
                    return Task::none();
                }
                let local = &mut Arc::make_mut(&mut self.durable).channels[index];
                local.ordinal = ordinal;
                // The registry answered about this channel, so it holds it. From
                // here its absence from a later model is a deletion rather than
                // ignorance, which is the whole point of recording the fact.
                local.shared = true;
                if let Some(shared) = self
                    .workspace_model
                    .channels
                    .iter_mut()
                    .find(|shared| shared.id == channel)
                {
                    shared.ordinal = ordinal;
                }
                self.sort_durable_channels();
                self.refresh_sidebar_channels();
                self.persist();
                Task::none()
            }
        }
    }

    pub(super) fn apply_closed_project_tombstones(&mut self) -> Task<Message> {
        let closed = self
            .workspace_model
            .closed_projects
            .iter()
            .filter(|repository| {
                !self.project_ui.closed.contains(*repository)
                    && !self.project_ui.is_reopening(repository)
            })
            .cloned()
            .collect::<Vec<_>>();
        Task::batch(
            closed
                .into_iter()
                .map(|repository| self.close_shared_project(&repository)),
        )
    }

    /// Settles what this window has shared with the registry it just reached.
    ///
    /// A [`ProjectChannel::shared`] flag only means anything relative to the
    /// registry it was set against, so an identity this window has not recorded
    /// means it has shared nothing at all: with a replaced daemon or a wiped
    /// data directory, every flag is a claim about a registry that no longer
    /// exists. Clearing them is what keeps the next reconciliation from reading
    /// a fresh registry's silence as a deletion and tombstoning the cards in
    /// every channel the user has.
    fn join_workspace_model(&mut self, identity: uuid::Uuid) {
        if self.durable.workspace_model_identity == Some(identity) {
            return;
        }
        self.shared_views = super::shared_views::State::for_workspace(&self.durable);
        self.clear_closed_chat_drafts();
        let durable = Arc::make_mut(&mut self.durable);
        durable.workspace_model_identity = Some(identity);
        for channel in durable.channels.iter_mut() {
            channel.shared = false;
        }
        self.persist();
    }

    /// Mirrors the cached shared model into this window's own store.
    ///
    /// A channel the registry lacks is decided by whether this window recorded
    /// having shared it. Never shared means new to the registry, so it is
    /// published; shared means it was deleted somewhere else, so it retires.
    /// The distinction is the whole join rule: before a window has shared a
    /// channel, the registry's silence about that channel carries no
    /// information, and retiring on it would tombstone the cards inside a
    /// channel nobody deleted.
    ///
    /// The first window to reach an empty registry therefore publishes
    /// everything it has, because nothing is flagged - the migration for a
    /// workspace that predates the registry, arrived at by the general rule
    /// rather than by a request of its own.
    ///
    /// Nothing happens at all until a registry has answered. The window
    /// reconciles on its own events too - a repository finishing loading is one
    /// - and the cached model starts out as a [`WorkspaceModel::default`], whose
    /// emptiness is not a registry's emptiness. Publishing against it would be
    /// premature and retiring against it would be destruction.
    pub(super) fn reconcile_workspace_model(&mut self) -> Task<Message> {
        if self.workspace_model.identity == NO_REGISTRY {
            return Task::none();
        }
        // Taken rather than cloned: reconciling needs `&mut self` for the
        // window's own store, and the model is put back untouched.
        let model = std::mem::take(&mut self.workspace_model);
        let mut tasks = Vec::new();
        let mut changed = false;
        // Projects first, so a channel's owner exists by the time the channel is
        // considered.
        for shared in &model.projects {
            if self.project_ui.closed.contains(&shared.repository) {
                continue;
            }
            match self.adopt_shared_project(shared) {
                Adoption::Applied => changed = true,
                Adoption::Opening(task) => tasks.push(task),
                Adoption::Retained => {}
            }
        }
        for project in &mut self.sidebar_model.projects {
            project.navigation_order = model
                .navigation_orders
                .iter()
                .find(|order| order.repository == project.repository)
                .map(|order| order.items.clone())
                .unwrap_or_default();
        }
        for shared in &model.channels {
            if self.channel_ui.is_deleting(shared.id) || self.channel_ui.was_deleted(shared.id) {
                continue;
            }
            match self
                .durable
                .channels
                .iter()
                .position(|local| local.id == shared.id)
            {
                Some(index) => {
                    if self.absorb_shared_channel(index, shared) {
                        changed = true;
                    }
                }
                None => match self.adopt_shared_channel(
                    shared,
                    model
                        .project(&shared.repository)
                        .map(|project| project.kind),
                ) {
                    Adoption::Applied => changed = true,
                    Adoption::Opening(task) => tasks.push(task),
                    Adoption::Retained => {}
                },
            }
        }
        let mut republished = Vec::new();
        let retired = self
            .durable
            .channels
            .iter()
            .filter(|local| !model.channels.iter().any(|shared| shared.id == local.id))
            .map(|local| (local.id, local.shared))
            .collect::<Vec<_>>();
        for (id, shared) in retired {
            if self.channel_ui.is_deleting(id) {
                continue;
            }
            if shared {
                // The window's own deletion path, so a channel that disappeared
                // elsewhere retires its cards exactly as a local delete retires
                // them.
                tasks.push(self.retire_peer_channel(id));
                changed = true;
            } else {
                republished.push(id);
            }
        }
        // Published in ascending local rank, so channels this window is
        // introducing keep their relative order under the fresh ranks the
        // registry assigns them. Those ranks are higher than everything the
        // registry already held, so a rejoining window's channels sort above
        // channels that may be older in wall-clock terms: a rank is the order
        // the registry learned of a channel, not the order a user made one, and
        // that is the only order two windows can agree on without a clock they
        // share.
        for id in republished {
            self.publish_shared_channel(id);
        }
        // Admission uses this incoming snapshot, not a blocked flag left by a
        // previous projection. Defer fresh actors while their scope is truly
        // blocked, before creating a local card or attachment Task.
        self.shared_views.blocked =
            super::shared_views::blocked_managed_view_scopes(&model, &model.views);
        let display_index = crate::workspace_model::views::ManagedDisplayIndex::new(&model);
        let mut new_managed_panes = Vec::new();
        for agent in &model.agents {
            // Scheduled agents stay headless until an explicit history or
            // notification action opens their exact live run.
            if matches!(
                agent.origin,
                crate::agent_execution::Origin::Automation { .. }
            ) {
                continue;
            }
            if let Some(display) = display_index.resolve(agent) {
                let card = CardId::from_uuid(agent.id.as_uuid());
                let unpublished = display.pane.is_none()
                    && !self.durable.cards.iter().any(|entry| entry.id == card);
                let scope = display.scope.clone();
                if unpublished && self.shared_views.blocked.contains(&scope) {
                    continue;
                }
                if let Some(task) = self.adopt_execution_agent_display(agent, display) {
                    if unpublished {
                        new_managed_panes.push((scope, card));
                    }
                    changed = true;
                    tasks.push(task);
                }
            }
        }
        self.workspace_model = model;
        // First adoption has no authoritative pane to project yet. Publish only
        // these exact new descriptors and overlay their pending Inserts before
        // passive projection; existing panes still project before persistence.
        for (scope, card) in new_managed_panes {
            self.publish_new_managed_pane(scope, card);
        }
        // Fence canonical evidence and finish projecting authoritative pane
        // sources before reconciliation persists. Publishing earlier would mix
        // old descriptors with newly received pending-start flags.
        tasks.push(self.reconcile_shared_views());
        if changed {
            self.sort_durable_channels();
            self.refresh_sidebar_channels();
            tasks.push(self.sync_chrome());
        }
        self.refresh_channel_title_suggestions();
        Task::batch(tasks)
    }

    /// Adoption records the daemon's exact descriptor without launching a
    /// replacement, switching channels, or taking focus from current work.
    pub(super) fn adopt_execution_agent(
        &mut self,
        agent: &crate::agent_execution::Agent,
    ) -> Option<Task<Message>> {
        let display = crate::workspace_model::views::managed_display(&self.workspace_model, agent)?;
        self.adopt_execution_agent_display(agent, display)
    }

    pub(super) fn adopt_execution_agent_display(
        &mut self,
        agent: &crate::agent_execution::Agent,
        display: crate::workspace_model::views::ManagedDisplay,
    ) -> Option<Task<Message>> {
        let session = match display.pane.as_ref().map(|pane| &pane.kind) {
            Some(CardKind::Chat(session)) => {
                crate::agent_execution::HostedSession::Chat(session.clone())
            }
            Some(CardKind::Terminal(session)) => {
                crate::agent_execution::HostedSession::Terminal(session.clone())
            }
            Some(_) => return None,
            None if agent.spec.surface == crate::agent_execution::Surface::Terminal => {
                crate::agent_execution::HostedSession::Terminal(agent.console.clone()?)
            }
            None => agent.session.clone()?,
        };
        self.adopt_execution_agent_resolved_session(agent, display, session)
    }

    pub(super) fn adopt_execution_agent_activation_display(
        &mut self,
        agent: &crate::agent_execution::Agent,
        display: crate::workspace_model::views::ManagedDisplay,
        session: crate::agent_execution::HostedSession,
    ) -> Option<Task<Message>> {
        use crate::agent_execution::HostedSession;
        if !session.has_complete_proof() {
            return None;
        }
        let owned = agent
            .session
            .as_ref()
            .is_some_and(|current| current.same_process(&session))
            || agent.console.as_ref().is_some_and(|current| {
                HostedSession::Terminal(current.clone()).same_process(&session)
            });
        let saved_matches = match display.pane.as_ref().map(|pane| &pane.kind) {
            Some(CardKind::Chat(current)) => {
                HostedSession::Chat(current.clone()).same_process(&session)
            }
            Some(CardKind::Terminal(current)) => {
                HostedSession::Terminal(current.clone()).same_process(&session)
            }
            Some(_) => false,
            None => true,
        };
        if !owned || !saved_matches {
            return None;
        }
        self.adopt_execution_agent_resolved_session(agent, display, session)
    }

    fn adopt_execution_agent_resolved_session(
        &mut self,
        agent: &crate::agent_execution::Agent,
        display: crate::workspace_model::views::ManagedDisplay,
        session: crate::agent_execution::HostedSession,
    ) -> Option<Task<Message>> {
        let placement = &display.scope;
        if !session.has_complete_proof()
            || agent.closed
            || matches!(placement, crate::agent_execution::Placement::Channel(channel) if self.channel_ui.is_deleting(*channel) || self.channel_ui.was_deleted(*channel))
        {
            return None;
        }
        let channel_index = match placement {
            crate::agent_execution::Placement::Channel(id) => Some(
                self.durable
                    .channels
                    .iter()
                    .position(|channel| channel.id == *id)?,
            ),
            crate::agent_execution::Placement::Worktree { path, .. } => {
                if self.project_ui.deleting_directory(path)
                    || self.worktree_view_scope(path).as_ref() != Some(placement)
                {
                    return None;
                }
                None
            }
        };
        let active = match placement {
            crate::agent_execution::Placement::Channel(id) => {
                self.durable.active_channel == Some(*id)
            }
            crate::agent_execution::Placement::Worktree { path, .. } => {
                self.durable.active_channel.is_none()
                    && self.active_workspace.as_ref() == Some(path)
            }
        };
        let card_id = CardId::from_uuid(agent.id.as_uuid());
        // Includes tombstones: an explicit local close is never undone by an
        // old model read, a reconnect, or another window observing this agent.
        if let Some(existing) = self.durable.cards.iter().find(|card| card.id == card_id) {
            if !existing.existence.is_open() {
                return None;
            }
            let current = match &existing.kind {
                CardKind::Chat(session) => {
                    crate::agent_execution::HostedSession::Chat(session.clone())
                }
                CardKind::Terminal(session) => {
                    crate::agent_execution::HostedSession::Terminal(session.clone())
                }
                _ => return None,
            };
            if !session.same_process(&current) {
                return None;
            }
            let saved = display.pane.as_ref()?;
            let in_place = match placement {
                crate::agent_execution::Placement::Channel(channel) => self
                    .durable
                    .channels
                    .iter()
                    .any(|owner| owner.id == *channel && owner.cards.contains(&card_id)),
                crate::agent_execution::Placement::Worktree { path, .. } => {
                    !self
                        .durable
                        .channels
                        .iter()
                        .any(|owner| owner.cards.contains(&card_id))
                        && self.durable.card_scopes.get(&card_id) == Some(path)
                }
            };
            if in_place {
                return None;
            }
            let durable = Arc::make_mut(&mut self.durable);
            durable
                .worktree_focus
                .retain(|_, focused| *focused != card_id);
            for channel in &mut durable.channels {
                channel.cards.retain(|id| *id != card_id);
                if channel.focused == Some(card_id) {
                    channel.focused = channel.cards.first().copied();
                }
            }
            if let Some(index) = channel_index {
                durable.channels[index].cards.push(card_id);
                durable.channels[index].focused.get_or_insert(card_id);
                self.worktree_owners.remove(&card_id);
                match &saved.scope {
                    Some(scope) => {
                        durable.card_scopes.insert(card_id, scope.clone());
                    }
                    None => {
                        durable.card_scopes.remove(&card_id);
                    }
                }
            } else if let crate::agent_execution::Placement::Worktree { path, .. } = placement {
                durable.card_scopes.insert(card_id, path.clone());
                durable
                    .worktree_focus
                    .entry(path.clone())
                    .or_insert(card_id);
                self.worktree_owners.insert(card_id, path.clone());
            }
            return Some(self.refresh_managed_display_strip());
        }
        let kind = match &session {
            crate::agent_execution::HostedSession::Chat(session) => CardKind::Chat(session.clone()),
            crate::agent_execution::HostedSession::Terminal(session) => {
                CardKind::Terminal(session.clone())
            }
        };
        let card = CardState {
            id: card_id,
            generation: self.durable.generation,
            title: display
                .pane
                .as_ref()
                .map(|pane| pane.title.clone())
                .unwrap_or_else(|| format!("{} agent", agent.spec.provider.label())),
            width_fraction: if let Some(pane) = &display.pane {
                pane.width_fraction
            } else if matches!(kind, CardKind::Chat(_)) {
                self.settings.default_native_pane_fraction
            } else {
                self.settings.default_column_fraction
            },
            existence: Existence::Open,
            exposure: Exposure::Offscreen,
            residency: Residency::Cold,
            kind,
        };
        let durable = Arc::make_mut(&mut self.durable);
        durable.cards.push(card.clone());
        let scope_path = match placement {
            crate::agent_execution::Placement::Worktree { path, .. } => path.clone(),
            crate::agent_execution::Placement::Channel(_) => display
                .pane
                .as_ref()
                .and_then(|pane| pane.scope.clone())
                .unwrap_or_else(|| agent.spec.target.directory.clone()),
        };
        durable.card_scopes.insert(card_id, scope_path.clone());
        if let Some(index) = channel_index {
            let channel = &mut durable.channels[index];
            channel.cards.push(card_id);
            channel.focused.get_or_insert(card_id);
        } else {
            durable
                .worktree_focus
                .entry(scope_path.clone())
                .or_insert(card_id);
            self.worktree_owners.insert(card_id, scope_path);
        }
        if let Some(pane) = &display.pane {
            match &pane.user_title {
                Some(title) => {
                    self.chrome_ui.user_titles.insert(card_id, title.clone());
                }
                None => {
                    self.chrome_ui.user_titles.remove(&card_id);
                }
            }
        }
        self.cards.insert(card_id, CardRuntime::Loading);
        self.lifecycle.register(
            card_id,
            Exposure::Offscreen,
            Residency::Cold,
            Instant::now(),
        );
        // Native chats keep a lightweight attachment even offscreen, just as
        // initialize_runtimes does after a window restart. The deferred
        // lifecycle hydrator only starts terminal/browser surfaces; leaving a
        // newly adopted chat Loading there would never subscribe to its actor.
        let attach = match &session {
            crate::agent_execution::HostedSession::Chat(session) => {
                self.open_chat(card_id, session.clone(), crate::chat::Intent::Restore)
            }
            crate::agent_execution::HostedSession::Terminal(_) => Task::none(),
        };
        if !active {
            return Some(attach);
        }
        let item = WorkspaceCard::new(
            card_id,
            card.title,
            CardWidth::Fraction(card.width_fraction),
        )
        .expect("execution title and default width are valid");
        let task = self
            .workspace
            .add_card(item, self.workspace.cards().len(), false)
            .map(|update| self.apply_viewport(update, true, false))
            .unwrap_or_else(|error| {
                self.status = Some(TransientNotice::error(error.to_string()));
                Task::none()
            });
        Some(Task::batch([attach, task]))
    }

    /// Takes the shared half of a channel this window already has.
    fn absorb_shared_channel(&mut self, index: usize, shared: &ModelChannel) -> bool {
        let local = &self.durable.channels[index];
        // The shared title wins even where this window's own derivation would
        // differ: one window deferring is better than two windows showing one
        // channel under two names.
        let retitle = local.title != shared.title || local.title_source != shared.title_source;
        // A rank the registry has not assigned yet must not overwrite one it
        // has, which is what a folded create carries until a read supplies the
        // real one.
        let rerank = shared.ordinal != PENDING_ORDINAL && local.ordinal != shared.ordinal;
        // The registry naming a channel is the proof this window is looking for:
        // from here its absence is a deletion rather than ignorance.
        let record = !local.shared;
        if !retitle && !rerank && !record {
            return false;
        }
        let channel = &mut Arc::make_mut(&mut self.durable).channels[index];
        if retitle {
            channel.title = shared.title.clone();
            channel.title_source = shared.title_source;
        }
        if rerank {
            channel.ordinal = shared.ordinal;
        }
        channel.shared = true;
        true
    }

    /// Shows a channel this window does not have.
    fn adopt_shared_channel(
        &mut self,
        shared: &ModelChannel,
        kind: Option<ProjectKind>,
    ) -> Adoption {
        let Some(project_id) = self.project_for_repository(&shared.repository) else {
            return self.open_shared_repository(&shared.repository, kind);
        };
        Arc::make_mut(&mut self.durable)
            .channels
            .push(ProjectChannel {
                title_source: shared.title_source,
                id: shared.id,
                project_id,
                title: shared.title.clone(),
                ordinal: shared.ordinal,
                // Adopted from the registry, so the registry demonstrably holds
                // it: this window may retire it when it later disappears.
                shared: true,
                // Adoption creates the scope, never a conversation. A local
                // creation opens one because the user just asked for somewhere
                // to talk; N windows adopting one channel must not start N
                // conversations inside it.
                cards: Vec::new(),
                focused: None,
            });
        self.sidebar_state.reveal_project(project_id);
        Adoption::Applied
    }

    /// Opens a project this window does not have.
    fn adopt_shared_project(&mut self, shared: &ModelProject) -> Adoption {
        if self.project_ui.closed.contains(&shared.repository) {
            return Adoption::Retained;
        }
        if let Some(project_id) = self.project_for_repository(&shared.repository) {
            if crate::device::is_remote()
                && !self.workspace_model_opens.contains(&shared.repository)
            {
                return Adoption::Opening(self.project_load_task(
                    shared.repository.clone(),
                    Some(project_id),
                    false,
                    Some(shared.kind),
                ));
            }
            return Adoption::Retained;
        }
        if crate::device::is_remote() {
            return self.adopt_device_project(shared);
        }
        self.open_shared_repository(&shared.repository, Some(shared.kind))
    }

    /// A device's model is already authoritative about its repository. Loading
    /// that path through the displaying computer would inspect another host's
    /// filesystem, and cannot supply any additional project metadata.
    fn adopt_device_project(&mut self, shared: &ModelProject) -> Adoption {
        if self.project_ui.closed.contains(&shared.repository)
            || self
                .sidebar_model
                .projects
                .iter()
                .any(|project| project.repository == shared.repository)
        {
            return Adoption::Retained;
        }
        let project = ProjectState {
            id: ProjectId::new(),
            generation: 0,
            name: shared.name.clone(),
            repository: shared.repository.clone(),
            kind: shared.kind,
            existence: Existence::Open,
        };
        if let Some(session) = &self.session {
            session.save_project(project.clone());
        }
        self.sidebar_model.projects.push(
            SidebarProject::new(
                project.id,
                RepositoryId(project.id.as_uuid()),
                project.name,
                project.repository,
                Vec::new(),
                Vec::new(),
            )
            .with_kind(project.kind),
        );
        self.sidebar_model
            .apply_project_order(&self.durable.project_order);
        self.sidebar_model.data_state = SidebarDataState::Ready;
        Adoption::Opening(self.project_load_task(
            shared.repository.clone(),
            Some(project.id),
            false,
            Some(shared.kind),
        ))
    }

    /// Reads a repository the shared model names and this window has not opened.
    fn open_shared_repository(&mut self, repository: &Path, kind: Option<ProjectKind>) -> Adoption {
        if crate::device::is_remote() {
            // Projects are adopted from their model metadata before channels.
            // Wait for that metadata instead of probing a device path locally.
            return Adoption::Retained;
        }
        // Once per path. `repository_load_task` mints a fresh `ProjectId` when
        // given none, so asking twice for one repository would leave the window
        // with two projects on one checkout.
        if !self.workspace_model_opens.insert(repository.to_path_buf()) {
            return Adoption::Retained;
        }
        if !repository.is_dir() {
            // Not a repository this window can read - it may live on a
            // filesystem that is not mounted here. The row stays in the model
            // and renders nothing, rather than becoming a project that cannot
            // be loaded. The key is a Git directory, so the directory itself is
            // what has to be there; testing `repository/.git` would test for a
            // Git directory inside a Git directory and never find one.
            return Adoption::Retained;
        }
        // Without cards. `open_cards` asks the load to select the repository's
        // worktree and start a terminal in it, which is what a user opening a
        // project in this window asked for and is exactly what an adoption did
        // not: the project was opened in another window or from a shell, and
        // the user of this window is in the middle of something else. Opening
        // cards here moved their active worktree out from under them and left a
        // terminal they never started - in every window at once. The project
        // still reaches the sidebar, because that is the load's own result
        // rather than a card's.
        Adoption::Opening(self.project_load_task(repository.to_path_buf(), None, false, kind))
    }

    /// The local project that is the face of `repository`.
    ///
    /// Compared canonically, because the shared key is canonical and a window
    /// may have opened the same checkout through a symlink.
    pub(super) fn project_for_repository(&self, repository: &Path) -> Option<ProjectId> {
        self.sidebar_model
            .projects
            .iter()
            .find(|project| {
                project.repository == repository
                    || !crate::device::is_remote()
                        && canonical_repository(&project.repository)
                            .is_ok_and(|canonical| canonical == repository)
            })
            .map(|project| project.id)
    }

    /// Keeps the durable channels in the registry's ascending rank order.
    ///
    /// The sidebar renders them reversed, so one sort here is what makes every
    /// window agree about which channel is on top. A stable sort is required:
    /// channels that share a rank - the unranked ones a registryless window
    /// creates - keep the order they were created in.
    pub(super) fn sort_durable_channels(&mut self) {
        if self
            .durable
            .channels
            .is_sorted_by_key(|channel| channel.ordinal)
        {
            return;
        }
        Arc::make_mut(&mut self.durable)
            .channels
            .sort_by_key(|channel| channel.ordinal);
    }

    /// The rank a channel created here takes until the registry answers.
    ///
    /// One more than the highest rank this window knows, which is what the
    /// registry itself would assign when the two are in step - and when they
    /// are not, the answer to the publish corrects it. A single sentinel rank
    /// would not do: two channels created before either answer arrived would
    /// tie, and the newer of them would not be the one on top.
    pub(super) fn next_provisional_ordinal(&self) -> u32 {
        self.durable
            .channels
            .iter()
            .map(|channel| channel.ordinal)
            .chain(
                self.workspace_model
                    .channels
                    .iter()
                    .map(|channel| channel.ordinal),
            )
            .max()
            .unwrap_or(0)
            .saturating_add(1)
    }

    /// Publishes this window's creation of a channel.
    pub(super) fn publish_channel_creation(&mut self, id: ChannelId, project_id: ProjectId) {
        let Some(project) = self.sidebar_model.project(project_id) else {
            return;
        };
        let repository = project.repository.clone();
        let name = project.name.clone();
        let kind = project.kind;
        // The project first: a channel in a repository no project owns is
        // reachable from no sidebar. Both edits go through one queue, so the
        // registry sees them in this order.
        self.publish_model_edit(ModelEdit::OpenProject {
            kind,
            repository: repository.clone(),
            name,
        });
        self.publish_model_edit(ModelEdit::CreateChannel {
            id,
            repository,
            title: String::new(),
            title_source: Default::default(),
        });
    }

    /// Publishes a channel this window holds and the registry has never heard
    /// of, which is how a window that predates the registry - or that was open
    /// while a replacement one was created - introduces what it already has.
    fn publish_shared_channel(&mut self, id: ChannelId) {
        let Some(channel) = self.durable.channels.iter().find(|local| local.id == id) else {
            return;
        };
        let title = channel.title.clone();
        let title_source = channel.title_source;
        let project_id = channel.project_id;
        let Some(project) = self.sidebar_model.project(project_id) else {
            return;
        };
        let repository = project.repository.clone();
        if self.project_ui.closed.contains(&repository) {
            return;
        }
        let name = project.name.clone();
        let kind = project.kind;
        self.publish_model_edit(ModelEdit::OpenProject {
            kind,
            repository: repository.clone(),
            name,
        });
        // Carrying the title this window already derived, unlike a fresh
        // creation: a channel being reintroduced has a name the user has seen,
        // and publishing it empty would blank it in every other window.
        self.publish_model_edit(ModelEdit::CreateChannel {
            id,
            repository,
            title,
            title_source,
        });
    }

    /// Publishes a project this window opened.
    pub(super) fn publish_project_opened(&mut self, project_id: ProjectId) {
        let Some(project) = self.sidebar_model.project(project_id) else {
            return;
        };
        if self.project_ui.closed.contains(&project.repository) {
            return;
        }
        let repository = project.repository.clone();
        let name = project.name.clone();
        let kind = project.kind;
        let edit = if self.project_ui.is_reopening(&repository) {
            ModelEdit::ReopenProject {
                repository,
                name,
                kind,
            }
        } else {
            ModelEdit::OpenProject {
                repository,
                name,
                kind,
            }
        };
        self.publish_model_edit(edit);
    }

    /// Queues one shared-model edit without waiting for it.
    ///
    /// A redraw must never wait on the daemon, so this only queues: the
    /// publisher thread owns the connection and the retry, and the window learns
    /// the outcome as an update like any other.
    pub(super) fn publish_model_edit(&mut self, edit: ModelEdit) {
        self.capture_view_update_source(&edit);
        let navigation = matches!(
            &edit,
            ModelEdit::ReorderNavigation { .. } | ModelEdit::ResetNavigation { .. }
        );
        if let Err(error) = self.workspace_model_tx.try_send(edit) {
            tracing::debug!(%error, "a shared workspace model edit was not queued");
            if navigation {
                self.status = Some(TransientNotice::warning(
                    "Could not save the sidebar order; try again once the selected device is connected",
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn busy_title_publication_yields_after_three_original_attempts_to_the_next_edit() {
        use crate::terminal::ipc::{
            ClientEnvelope, ClientRequest, PROTOCOL_VERSION, ProtocolError, ProtocolErrorCode,
            ServerEnvelope, ServerMessage, ServerResponse, read_frame, write_frame,
        };
        let id = ChannelId::new();
        let title = ModelEdit::SuggestConversationTitle {
            id,
            card: crate::model::CardId::new(),
            expected_revision: 17,
            title: "Encryption review".into(),
            source: crate::model::ChannelTitleSource::Provider,
        };
        let manual = ModelEdit::RetitleChannel {
            id,
            title: "My project".into(),
        };
        let expected_title = title.clone();
        let expected_manual = manual.clone();
        let (client, mut peer) = std::os::unix::net::UnixStream::pair().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let server = std::thread::spawn(move || {
            let proof = crate::model::HostSessionProof {
                host_session_id: "title-publisher".into(),
                boot_id: "fixture-boot".into(),
                host_incarnation: "fixture-incarnation".into(),
            };
            let hello: ClientEnvelope = read_frame(&mut peer).unwrap().unwrap();
            write_frame(
                &mut peer,
                &ServerEnvelope {
                    version: PROTOCOL_VERSION,
                    request_id: Some(hello.request_id),
                    proof: proof.clone(),
                    message: ServerMessage::Response {
                        response: ServerResponse::Connected {
                            proof: proof.clone(),
                            resumed: vec![],
                        },
                    },
                },
            )
            .unwrap();
            for _ in 0..3 {
                let request: ClientEnvelope = read_frame(&mut peer).unwrap().unwrap();
                assert_eq!(
                    request.request,
                    ClientRequest::EditWorkspaceModel {
                        edit: expected_title.clone()
                    }
                );
                write_frame(
                    &mut peer,
                    &ServerEnvelope {
                        version: PROTOCOL_VERSION,
                        request_id: Some(request.request_id),
                        proof: proof.clone(),
                        message: ServerMessage::Error(ProtocolError::new(
                            ProtocolErrorCode::QueueFull,
                            "Resident metadata is busy",
                            true,
                        )),
                    },
                )
                .unwrap();
            }
            let read: ClientEnvelope = read_frame(&mut peer).unwrap().unwrap();
            assert_eq!(read.request, ClientRequest::ReadWorkspaceModel);
            write_frame(
                &mut peer,
                &ServerEnvelope {
                    version: PROTOCOL_VERSION,
                    request_id: Some(read.request_id),
                    proof: proof.clone(),
                    message: ServerMessage::Response {
                        response: ServerResponse::WorkspaceModel {
                            model: WorkspaceModel::default(),
                        },
                    },
                },
            )
            .unwrap();
            let next: ClientEnvelope = read_frame(&mut peer).unwrap().unwrap();
            assert_eq!(
                next.request,
                ClientRequest::EditWorkspaceModel {
                    edit: expected_manual
                }
            );
            write_frame(
                &mut peer,
                &ServerEnvelope {
                    version: PROTOCOL_VERSION,
                    request_id: Some(next.request_id),
                    proof: proof.clone(),
                    message: ServerMessage::Response {
                        response: ServerResponse::WorkspaceModel {
                            model: WorkspaceModel::default(),
                        },
                    },
                },
            )
            .unwrap();
            assert!(
                read_frame::<ClientEnvelope>(&mut peer).unwrap().is_none(),
                "No title replay after yielding"
            );
        });
        let mut client = ControlClient::from_stream(client).unwrap();
        let (sender, updates) = async_channel::unbounded();
        send(&mut client, title.clone(), &sender).unwrap();
        assert!(matches!(updates.try_recv().unwrap(), ModelUpdate::Read(_)));
        assert!(
            matches!(updates.try_recv().unwrap(), ModelUpdate::TitleDeferred(edit) if edit == title)
        );
        send(&mut client, manual, &sender).unwrap();
        drop(client);
        server.join().unwrap();
    }

    #[test]
    fn a_source_conflict_drains_the_stale_request_without_an_applied_receipt() {
        use crate::terminal::ipc::{
            ClientEnvelope, ClientRequest, PROTOCOL_VERSION, ProtocolError, ProtocolErrorCode,
            ServerEnvelope, ServerMessage, ServerResponse, read_frame, write_frame,
        };
        use crate::workspace_model::views::{Change, Pane, Scope, View};
        use std::os::unix::net::UnixStream;
        let app = super::super::tests::context_inspector_test_app(true);
        let before = Pane::from_card(&app.durable.cards[0], None).unwrap();
        let scope = Scope::Channel(ChannelId::new());
        let mut desired = before.clone();
        desired.pending_terminal_start = true;
        desired.title = "Captured title".into();
        let stale = uuid::Uuid::new_v4();
        let next = uuid::Uuid::new_v4();
        let change = Change::Update {
            before: before.clone(),
            pane: desired,
        };
        let mut current = before;
        current.scope = Some("/changed-source".into());
        let model = WorkspaceModel {
            views: vec![View {
                scope: scope.clone(),
                revision: 2,
                panes: vec![current.clone()],
                retired: vec![],
            }],
            ..WorkspaceModel::default()
        };
        let mut presentation = current.clone();
        presentation.title = "Independent next request".into();
        let next_edit = ModelEdit::ChangeView {
            scope: scope.clone(),
            expected_revision: 2,
            operation: next,
            change: Change::Update {
                before: current,
                pane: presentation,
            },
        };
        let first_edit = ModelEdit::ChangeView {
            scope: scope.clone(),
            expected_revision: 1,
            operation: stale,
            change: change.clone(),
        };
        let (client_stream, mut server_stream) = UnixStream::pair().unwrap();
        server_stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let expected_model = model.clone();
        let server = std::thread::spawn(move || {
            let proof = crate::model::HostSessionProof {
                host_session_id: crate::model::HostSessionId::new("conflict-fixture"),
                boot_id: crate::model::BootId::new("conflict-boot"),
                host_incarnation: crate::model::HostIncarnation::new("conflict-host"),
            };
            let envelope = |request_id, message| ServerEnvelope {
                version: PROTOCOL_VERSION,
                request_id: Some(request_id),
                proof: proof.clone(),
                message,
            };
            let connect: ClientEnvelope = read_frame(&mut server_stream).unwrap().unwrap();
            assert!(matches!(connect.request, ClientRequest::Connect { .. }));
            write_frame(
                &mut server_stream,
                &envelope(
                    connect.request_id,
                    ServerMessage::Response {
                        response: ServerResponse::Connected {
                            proof: proof.clone(),
                            resumed: vec![],
                        },
                    },
                ),
            )
            .unwrap();
            let edit: ClientEnvelope = read_frame(&mut server_stream).unwrap().unwrap();
            assert!(matches!(edit.request, ClientRequest::EditWorkspaceModel {
                edit: ModelEdit::ChangeView { operation, .. } } if operation == stale));
            write_frame(
                &mut server_stream,
                &envelope(
                    edit.request_id,
                    ServerMessage::Error(ProtocolError::new(
                        ProtocolErrorCode::ViewConflict,
                        "The pane source changed",
                        true,
                    )),
                ),
            )
            .unwrap();
            let read: ClientEnvelope = read_frame(&mut server_stream).unwrap().unwrap();
            assert!(matches!(read.request, ClientRequest::ReadWorkspaceModel));
            write_frame(
                &mut server_stream,
                &envelope(
                    read.request_id,
                    ServerMessage::Response {
                        response: ServerResponse::WorkspaceModel {
                            model: model.clone(),
                        },
                    },
                ),
            )
            .unwrap();
            // A revision-only retry would repeat stale here and fail. The
            // publisher must instead be free to admit the next independent edit.
            let edit: ClientEnvelope = read_frame(&mut server_stream).unwrap().unwrap();
            assert!(matches!(edit.request, ClientRequest::EditWorkspaceModel {
                edit: ModelEdit::ChangeView { operation, .. } } if operation == next));
            write_frame(
                &mut server_stream,
                &envelope(
                    edit.request_id,
                    ServerMessage::Response {
                        response: ServerResponse::WorkspaceModel { model },
                    },
                ),
            )
            .unwrap();
            assert!(
                read_frame::<ClientEnvelope>(&mut server_stream)
                    .unwrap()
                    .is_none()
            );
        });
        let mut client = ControlClient::from_stream(client_stream).unwrap();
        let (tx, rx) = async_channel::unbounded();
        send(&mut client, first_edit, &tx).unwrap();
        assert!(
            matches!(rx.try_recv().unwrap(), ModelUpdate::ViewConflicted {
            operation, change: actual, model, ..
        } if operation == stale && actual == change && model == expected_model)
        );
        assert!(
            rx.try_recv().is_err(),
            "a refused source edit has no ViewApplied receipt"
        );
        send(&mut client, next_edit, &tx).unwrap();
        assert!(matches!(rx.try_recv().unwrap(), ModelUpdate::ViewApplied {
            operation: Some(operation), ..
        } if operation == next));
        drop(client);
        server.join().unwrap();
    }

    #[test]
    fn device_project_metadata_creates_durable_sidebar_without_local_filesystem() {
        let mut app = super::super::tests::context_inspector_test_app(true);
        let storage = tempfile::tempdir().unwrap();
        let database = storage.path().join("state.sqlite3");
        let boot = crate::model::BootId::new("device-adoption");
        let (session, _) = crate::session::SessionRuntime::open(&database, boot.clone()).unwrap();
        app.session = Some(session);
        let focus = app.workspace.focused();
        let active = app.durable.active_channel;
        let cards = app.durable.cards.clone();
        let repository = PathBuf::from(format!(
            "/device-only/{}/repository/.git",
            uuid::Uuid::new_v4()
        ));
        assert!(
            !repository.exists(),
            "fixture must be absent on the displaying host"
        );
        let metadata = ModelProject {
            repository: repository.clone(),
            name: "Device repository".into(),
            kind: ProjectKind::Git,
            ordinal: 1,
        };
        assert!(matches!(
            app.adopt_device_project(&metadata),
            Adoption::Opening(_)
        ));
        let row = app
            .sidebar_model
            .projects
            .iter()
            .find(|row| row.repository == repository)
            .unwrap();
        let id = row.id;
        assert_eq!(row.name, metadata.name);
        let channel = shared(ChannelId::new(), &repository, "Remote work", 0);
        assert!(matches!(
            app.adopt_shared_channel(&channel, Some(ProjectKind::Git)),
            Adoption::Applied
        ));
        assert!(
            app.durable
                .channels
                .iter()
                .any(|local| local.id == channel.id && local.project_id == id)
        );
        assert!(matches!(
            app.adopt_device_project(&metadata),
            Adoption::Retained
        ));
        assert_eq!(
            app.sidebar_model
                .projects
                .iter()
                .filter(|project| project.id == id)
                .count(),
            1
        );
        assert_eq!(app.workspace.focused(), focus);
        assert_eq!(app.durable.active_channel, active);
        assert_eq!(app.durable.cards, cards);
        let mut session = app.session.take().unwrap();
        session.shutdown();
        assert_eq!(session.last_error(), None);
        let (mut restored, _) = crate::session::SessionRuntime::open(&database, boot).unwrap();
        let (projects, _) = restored.take_catalog();
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].id, id);
        assert_eq!(projects[0].repository, repository);
        assert_eq!(projects[0].name, metadata.name);
        assert_eq!(projects[0].kind, ProjectKind::Git);
        assert!(projects[0].existence.is_open());
        restored.shutdown();
        app.project_ui.closed.insert(repository.clone());
        app.sidebar_model.projects.retain(|row| row.id != id);
        assert!(matches!(
            app.adopt_device_project(&metadata),
            Adoption::Retained
        ));
        assert!(
            app.sidebar_model
                .projects
                .iter()
                .all(|row| row.repository != repository)
        );
    }

    #[test]
    fn worktree_execution_adopts_into_exact_strip_without_a_channel_or_focus_change() {
        use crate::agent_execution::{Agent, Origin, Placement, Spec, Surface, Target};
        let (mut app, _, repository) = app_with_project();
        let path = PathBuf::from("/repo");
        app.active_workspace = Some(path.clone());
        Arc::make_mut(&mut app.durable).active_channel = None;
        let focus = app.workspace.focused();
        let console = TerminalSession {
            id: TerminalId::new(),
            owned_codex_listener: None,
            recovery_origin: None,
            predecessor: None,
            launch: LaunchRecipe {
                program: "claude".into(),
                arguments: vec![],
                working_directory: "/repo/nested".into(),
                environment: Default::default(),
                unset_environment: Default::default(),
            },
            transcript_path: None,
            resume_hint: None,
            continuity: TerminalContinuity::Live {
                host_session_id: crate::model::HostSessionId::new("placement-test"),
                boot_id: crate::model::BootId::new("placement-test"),
                host_incarnation: crate::model::HostIncarnation::new("placement-test"),
            },
        };
        let placement = Placement::Worktree {
            repository: repository.clone(),
            path: path.clone(),
        };
        let agent = Agent {
            name: None,
            id: crate::model::AgentId::new(),
            origin: Origin::User,
            spec: Spec {
                provider: crate::agent::Provider::Claude,
                account: None,
                target: Target {
                    repository,
                    directory: "/repo/nested".into(),
                    placement: Some(placement.clone()),
                },
                surface: Surface::Terminal,
                model: None,
                permission_mode: None,
                reasoning_effort: None,
            },
            placement,
            session: None,
            console: Some(console),
            created_ms: 0,
            closed: false,
        };
        let card = CardId::from_uuid(agent.id.as_uuid());
        let channels = app.durable.channels.clone();
        let deletion = app.project_ui.begin_deleting(path.clone());
        assert!(app.adopt_execution_agent(&agent).is_none());
        drop(app.update(Message::ProjectUi(
            crate::app::project_ui::Action::Deleted {
                capture: app.capture_worktree_removal(&deletion),
                target: deletion,
                result: Err("fixture deletion refused".into()),
            },
        )));
        assert!(app.adopt_execution_agent(&agent).is_some());
        assert_eq!(app.durable.channels, channels);
        assert_eq!(app.workspace.focused(), focus);
        assert!(app.workspace.cards().iter().any(|pane| pane.id == card));
        assert_eq!(app.durable.card_scopes.get(&card), Some(&path));
        assert_eq!(app.worktree_owners.get(&card), Some(&path));
        assert!(
            app.adopt_execution_agent(&agent).is_none(),
            "replayed model does not mount twice"
        );
    }

    #[test]
    fn first_model_read_uses_moved_native_pane_instead_of_stale_managed_placement() {
        use crate::agent::Provider;
        use crate::agent_execution::{Agent, HostedSession, Origin, Spec, Surface, Target};
        let (mut app, project, repository) = app_with_project();
        let active = ChannelId::new();
        let target = ChannelId::new();
        let focus = app.workspace.focused();
        let mailbox_project = ProjectId::new();
        app.sidebar_model.projects.push(SidebarProject::new(
            mailbox_project,
            RepositoryId::new(),
            "Mailbox".into(),
            "/other-project".into(),
            Vec::new(),
            Vec::new(),
        ));
        let durable = Arc::make_mut(&mut app.durable);
        durable
            .channels
            .push(local(active, mailbox_project, "Active", 0));
        durable
            .channels
            .push(local(target, project, "Execution", 1));
        durable.active_channel = Some(active);
        let session = crate::chat::Session {
            native_continuity: Default::default(),
            id: TerminalId::new(),
            proof: Some(crate::model::HostSessionProof {
                host_session_id: crate::model::HostSessionId::new("execution-host"),
                boot_id: crate::model::BootId::new("execution-boot"),
                host_incarnation: crate::model::HostIncarnation::new("execution-incarnation"),
            }),
            terminal: None,
            owned_codex_listener: None,
            launch: crate::chat::Launch {
                provider: Provider::Claude,
                session_id: None,
                recipe: LaunchRecipe {
                    program: "claude".into(),
                    arguments: Vec::new(),
                    working_directory: PathBuf::from("/repo/worktree"),
                    environment: Default::default(),
                    unset_environment: Default::default(),
                },
            },
        };
        let agent = Agent {
            name: None,
            id: crate::model::AgentId::new(),
            origin: Origin::User,
            spec: Spec {
                provider: Provider::Claude,
                account: None,
                target: Target {
                    repository,
                    directory: PathBuf::from("/repo/worktree"),
                    placement: Some(crate::agent_execution::Placement::Channel(target)),
                },
                surface: Surface::Chat,
                model: None,
                permission_mode: None,
                reasoning_effort: None,
            },
            placement: crate::agent_execution::Placement::Channel(target),
            session: Some(HostedSession::Chat(session.clone())),
            console: None,
            created_ms: 0,
            closed: false,
        };
        let card = CardId::from_uuid(agent.id.as_uuid());
        let mut model = registry(vec![
            shared(active, &agent.spec.target.repository, "Mailbox", 0),
            shared(target, &agent.spec.target.repository, "Original", 1),
        ]);
        // Display membership is independent of execution ownership: legacy
        // protocol-77 transfers could place a cagents actor in another project.
        model.channels[0].repository = "/other-project".into();
        model.projects = vec![
            ModelProject {
                repository: agent.spec.target.repository.clone(),
                name: "Execution".into(),
                kind: ProjectKind::Git,
                ordinal: 0,
            },
            ModelProject {
                repository: "/other-project".into(),
                name: "Mailbox".into(),
                kind: ProjectKind::Git,
                ordinal: 1,
            },
        ];
        model.agents.push(agent.clone());
        model.views.push(crate::workspace_model::views::View {
            scope: crate::workspace_model::views::Scope::Channel(active),
            revision: 2,
            panes: vec![crate::workspace_model::views::Pane {
                id: card,
                title: "Mailbox lead".into(),
                user_title: Some("Mailbox lead".into()),
                pending_terminal_start: false,
                width_fraction: 0.5,
                kind: CardKind::Chat(session.clone()),
                scope: Some(agent.spec.target.directory.clone()),
            }],
            retired: vec![],
        });
        model.views.push(crate::workspace_model::views::View {
            scope: agent.placement.clone(),
            revision: 2,
            panes: vec![],
            retired: vec![card],
        });
        assert!(app.workspace_model.views.is_empty());
        app.workspace_model = model;
        drop(app.reconcile_workspace_model());
        assert!(
            app.durable
                .channels
                .iter()
                .find(|channel| channel.id == active)
                .unwrap()
                .cards
                .contains(&card)
        );
        assert!(
            !app.durable
                .channels
                .iter()
                .find(|channel| channel.id == target)
                .unwrap()
                .cards
                .contains(&card)
        );
        assert_eq!(
            app.durable.card_scopes.get(&card),
            Some(&agent.spec.target.directory)
        );
        assert_eq!(
            app.workspace_model.agents[0], agent,
            "display adoption must not rewrite execution authority"
        );
        assert!(app.chat_start_pending(card));
        assert_eq!(app.workspace.focused(), focus);
        assert!(
            matches!(&app.card_state(card).unwrap().kind, CardKind::Chat(current) if current == &session)
        );
        // Open Session in automation history must select this same display,
        // even when its retained agent record still names the original target.
        use crate::automation_ui::{Action, Effect};
        use crate::automations::{
            Definition, Firing, FiringState, RunDetail, Schedule, SchedulePattern, SessionMode,
            Trigger,
        };
        let automation = uuid::Uuid::new_v4();
        let mut automated = agent.clone();
        automated.origin = Origin::Automation { id: automation };
        let detail = RunDetail {
            definition: Definition {
                name: "Mailbox".into(),
                prompt: "Review".into(),
                agent: automated.spec.clone(),
                schedule: Schedule {
                    pattern: SchedulePattern::Weekly {
                        weekday: 1,
                        hour: 9,
                        minute: 0,
                    },
                    timezone: "UTC".into(),
                },
                session_mode: SessionMode::Reuse,
                precheck: None,
            },
            firing: Firing {
                id: uuid::Uuid::new_v4(),
                automation,
                name: "Mailbox".into(),
                revision: 1,
                trigger: Trigger::Manual {
                    request: uuid::Uuid::new_v4(),
                },
                state: FiringState::Running,
                release_pending: false,
                cancel_requested: false,
                created_ms: 0,
                started_ms: Some(0),
                finished_ms: None,
                agent: Some(automated.id),
                run: None,
                skip: None,
                diagnostic: None,
            },
            agent: Some(automated),
            run: None,
        };
        let Some(Effect::Request { sequence, .. }) = app
            .automation_ui
            .update(Action::InspectRun(crate::model::RunId::new()))
        else {
            panic!("run request expected")
        };
        app.automation_ui.update(Action::Received {
            sequence,
            result: Arc::new(Ok(crate::automations::Response::Detail(detail.clone()))),
        });
        let Some(Effect::Request { sequence, .. }) = app.automation_ui.update(Action::OpenSession)
        else {
            panic!("open request expected")
        };
        Arc::make_mut(&mut app.durable).active_channel = Some(target);
        drop(app.update_automation_ui(Action::Received {
            sequence,
            result: Arc::new(Ok(crate::automations::Response::Detail(detail))),
        }));
        assert_eq!(
            app.durable.active_channel,
            Some(active),
            "history opens the actual moved pane, not the stale launch channel"
        );
    }

    #[test]
    fn model_read_publishes_new_managed_chat_before_initialized_view_projection() {
        use crate::agent::Provider;
        use crate::agent_execution::{Agent, HostedSession, Origin, Spec, Surface, Target};
        use crate::workspace_model::views::{Change, Scope, View};
        for (active_target, retired, blocked_before, metadata_edge) in [
            (true, false, false, 0),
            (false, false, false, 0),
            (true, true, false, 0),
            (true, false, true, 0),
            (true, false, false, 1),
            (true, false, false, 2),
            (true, false, false, 3),
            (true, false, false, 4),
            (true, false, false, 5),
            (true, false, false, 6),
            (true, false, false, 7),
        ] {
            let existing_primary = metadata_edge != 0;
            let (mut app, project, repository) = app_with_project();
            let active = ChannelId::new();
            let target = if active_target {
                active
            } else {
                ChannelId::new()
            };
            let durable = Arc::make_mut(&mut app.durable);
            durable.channels.push(local(active, project, "Active", 0));
            if target != active {
                durable
                    .channels
                    .push(local(target, project, "Execution", 1));
            }
            durable.active_channel = Some(active);
            let session = crate::chat::Session {
                native_continuity: Default::default(),
                id: TerminalId::new(),
                proof: Some(crate::model::HostSessionProof {
                    host_session_id: crate::model::HostSessionId::new("execution-host"),
                    boot_id: crate::model::BootId::new("execution-boot"),
                    host_incarnation: crate::model::HostIncarnation::new("execution-incarnation"),
                }),
                terminal: None,
                owned_codex_listener: None,
                launch: crate::chat::Launch {
                    provider: Provider::Claude,
                    session_id: None,
                    recipe: LaunchRecipe {
                        program: "claude".into(),
                        arguments: Vec::new(),
                        working_directory: PathBuf::from("/repo/worktree"),
                        environment: Default::default(),
                        unset_environment: Default::default(),
                    },
                },
            };
            let agent = Agent {
                name: None,
                id: crate::model::AgentId::new(),
                origin: Origin::User,
                spec: Spec {
                    provider: Provider::Claude,
                    account: None,
                    target: Target {
                        repository,
                        directory: PathBuf::from("/repo/worktree"),
                        placement: Some(crate::agent_execution::Placement::Channel(target)),
                    },
                    surface: Surface::Chat,
                    model: None,
                    permission_mode: None,
                    reasoning_effort: None,
                },
                placement: crate::agent_execution::Placement::Channel(target),
                session: Some(HostedSession::Chat(session.clone())),
                console: None,
                created_ms: 0,
                closed: false,
            };
            let card = CardId::from_uuid(agent.id.as_uuid());
            let scope = Scope::Channel(target);
            let mut channels = vec![shared(active, &agent.spec.target.repository, "Active", 0)];
            if target != active {
                channels.push(shared(
                    target,
                    &agent.spec.target.repository,
                    "Execution",
                    1,
                ));
            }
            let mut model = registry(channels);
            if existing_primary {
                model
                    .channels
                    .iter_mut()
                    .find(|channel| channel.id == target)
                    .unwrap()
                    .title_source = crate::model::ChannelTitleSource::Prompt;
            }
            let mut view = View::empty(scope.clone());
            view.revision = 17;
            if retired {
                view.retired.push(card);
            }
            model.views.push(view);
            if existing_primary {
                app.active_workspace = Some(PathBuf::from("/repo"));
            }
            app.workspace.resize_viewport(1000.0);
            let primary = existing_primary.then(|| {
                let mut primary = agent.clone();
                primary.id = crate::model::AgentId::new();
                let mut primary_session = session.clone();
                primary_session.id = TerminalId::new();
                primary.session = Some(HostedSession::Chat(primary_session.clone()));
                let id = CardId::from_uuid(primary.id.as_uuid());
                model.views[0]
                    .panes
                    .push(crate::workspace_model::views::Pane {
                        id,
                        title: "Existing primary".into(),
                        user_title: Some("Existing primary".into()),
                        pending_terminal_start: false,
                        width_fraction: 0.5,
                        kind: CardKind::Chat(primary_session),
                        scope: Some(primary.spec.target.directory.clone()),
                    });
                model.agents.push(primary);
                id
            });
            app.workspace_model = model.clone();
            let (tx, rx) = async_channel::unbounded();
            app.workspace_model_tx = tx;
            // This view already exists and its real seed acknowledgement has
            // completed before the daemon announces the new live Agent.
            app.acknowledge_view_edit(scope.clone(), None);
            if let Some(primary) = primary {
                drop(app.apply_workspace_model_update(ModelUpdate::Read(model.clone())));
                assert_eq!(app.workspace.order().collect::<Vec<_>>(), vec![primary]);
                assert_eq!(app.workspace.focused(), Some(primary));
            }
            while rx.try_recv().is_ok() {}
            if blocked_before {
                let mut conflict_agent = agent.clone();
                conflict_agent.id = crate::model::AgentId::new();
                conflict_agent.origin = Origin::Automation {
                    id: uuid::Uuid::new_v4(),
                };
                let mut foreign_session = session.clone();
                foreign_session.id = TerminalId::new();
                let conflict = crate::workspace_model::views::Pane {
                    id: CardId::from_uuid(conflict_agent.id.as_uuid()),
                    title: "Conflicting saved descriptor".into(),
                    user_title: None,
                    pending_terminal_start: false,
                    width_fraction: 0.72,
                    kind: CardKind::Chat(foreign_session),
                    scope: Some(agent.spec.target.directory.clone()),
                };
                model.views[0].panes.push(conflict);
                model.agents.push(conflict_agent);
                model.sequence += 1;
                drop(app.apply_workspace_model_update(ModelUpdate::Read(model.clone())));
                assert!(app.shared_views.blocked.contains(&scope));
                model.agents.push(agent.clone());
                model.sequence += 1;
                drop(app.apply_workspace_model_update(ModelUpdate::Read(model.clone())));
                assert!(
                    app.card_state(card).is_none(),
                    "a truly blocked scope must not create a fresh card or Starting Task"
                );
                assert!(!app.chat_start_pending(card));
                assert!(!std::iter::from_fn(|| rx.try_recv().ok()).any(|edit|
                    matches!(edit, ModelEdit::ChangeView { change: Change::Insert { pane, .. }, .. } if pane.id == card)));
                // The next authoritative read removes the conflict while the
                // previous projection's blocked flag is still set locally.
                model.views[0].panes.clear();
                model.agents.retain(|existing| existing.id == agent.id);
            } else {
                model.agents.push(agent.clone());
            }
            model.sequence += 1;
            let focus = app.workspace.focused();
            drop(app.apply_workspace_model_update(ModelUpdate::Read(model.clone())));
            let edits = std::iter::from_fn(|| rx.try_recv().ok()).collect::<Vec<_>>();
            let inserts = edits
                .iter()
                .filter_map(|edit| match edit {
                    ModelEdit::ChangeView {
                        scope: actual,
                        operation,
                        expected_revision,
                        change: Change::Insert { pane, before },
                    } if pane.id == card => {
                        Some((actual, operation, expected_revision, pane, before))
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            if retired {
                assert!(
                    inserts.is_empty(),
                    "authoritative retirement grants no new publication"
                );
                assert!(app.card_state(card).is_none());
                continue;
            }
            assert_eq!(
                inserts.len(),
                1,
                "exact first adoption must publish once before passive removal"
            );
            let (actual, operation, revision, pane, before) = inserts[0];
            assert_eq!(actual, &scope);
            assert_eq!(*revision, 17);
            assert_eq!(*before, None);
            assert!(matches!(&pane.kind, CardKind::Chat(actual) if actual == &session));
            assert!(app.card_state(card).unwrap().existence.is_open());
            assert!(
                app.durable
                    .channels
                    .iter()
                    .find(|channel| channel.id == target)
                    .unwrap()
                    .cards
                    .contains(&card)
            );
            assert!(
                app.chat_start_pending(card),
                "unpolled real attachment Task remains Starting, not detached"
            );
            assert!(matches!(app.cards.get(&card), Some(CardRuntime::Loading)));
            assert_eq!(app.workspace_model.agents.last(), Some(&agent));
            if !active_target || existing_primary {
                assert_eq!(app.workspace.focused(), focus);
            }
            if let Some(primary) = primary {
                assert_eq!(
                    app.workspace.order().collect::<Vec<_>>(),
                    vec![primary, card]
                );
            }
            // Repeated daemon snapshots preserve the exact pending Insert,
            // rather than re-issuing it or treating the old empty view as close.
            drop(app.apply_workspace_model_update(ModelUpdate::Read(model.clone())));
            assert!(app.card_state(card).unwrap().existence.is_open());
            assert!(!std::iter::from_fn(|| rx.try_recv().ok()).any(|edit|
                matches!(edit, ModelEdit::ChangeView { change: Change::Insert { pane, .. }, .. } if pane.id == card)));
            let change = Change::Insert {
                pane: pane.clone(),
                before: None,
            };
            model
                .views
                .iter_mut()
                .find(|view| view.scope == scope)
                .unwrap()
                .apply(&change)
                .unwrap();
            model.sequence += 1;
            model
                .views
                .iter_mut()
                .find(|view| view.scope == scope)
                .unwrap()
                .revision += 1;
            let edge = existing_primary.then(|| match metadata_edge {
                1 => ModelEdit::AgentsChanged,
                2 => ModelEdit::RetitleChannel {
                    id: target,
                    title: "New manual title".into(),
                },
                3 => ModelEdit::SuggestChannelTitle {
                    id: target,
                    title: "New provider title".into(),
                    source: crate::model::ChannelTitleSource::Provider,
                },
                4 => ModelEdit::SuggestConversationTitle {
                    id: target,
                    card: primary.unwrap(),
                    expected_revision: model.views[0].revision,
                    title: "New conversation title".into(),
                    source: crate::model::ChannelTitleSource::Provider,
                },
                5 => ModelEdit::CreateChannel {
                    id: ChannelId::new(),
                    repository: agent.spec.target.repository.clone(),
                    title: "Independent channel".into(),
                    title_source: Default::default(),
                },
                6 => ModelEdit::OpenProject {
                    repository: "/other/.git".into(),
                    name: "Other".into(),
                    kind: ProjectKind::Git,
                },
                7 => ModelEdit::ReopenProject {
                    repository: "/other/.git".into(),
                    name: "Other".into(),
                    kind: ProjectKind::Git,
                },
                _ => unreachable!(),
            });
            if let Some(edit) = &edge {
                // The observer and publisher use distinct connections. A
                // metadata edge may arrive before this Insert's own reply.
                if matches!(edit, ModelEdit::SuggestConversationTitle { .. }) {
                    // The real observer reads this proof-dependent edge as a
                    // complete snapshot, not a fold against its stale view17.
                    let mut observed = model.clone();
                    observed.fold(model.sequence + 1, edit);
                    drop(app.apply_workspace_model_update(ModelUpdate::Read(observed)));
                } else {
                    drop(app.apply_workspace_model_update(ModelUpdate::Edited {
                        sequence: model.sequence + 1,
                        edit: edit.clone(),
                    }));
                }
            }
            let newer_metadata = (
                app.workspace_model.channels.clone(),
                app.workspace_model.projects.clone(),
            );
            let accepted_snapshot = model.clone();
            drop(app.apply_workspace_model_update(ModelUpdate::ViewApplied {
                scope: scope.clone(),
                operation: Some(*operation),
                model: model.clone(),
            }));
            if let Some(primary) = primary {
                assert_eq!(
                    app.workspace.order().collect::<Vec<_>>(),
                    vec![primary, card]
                );
                assert_eq!(app.workspace.focused(), Some(primary));
                assert_eq!(app.workspace_model.channels, newer_metadata.0);
                assert_eq!(app.workspace_model.projects, newer_metadata.1);
                let expected_title = match metadata_edge {
                    2 => Some("New manual title"),
                    3 => Some("New provider title"),
                    4 => Some("New conversation title"),
                    _ => None,
                };
                if let Some(title) = expected_title {
                    assert_eq!(app.workspace_model.channel(target).unwrap().title, title);
                    assert_eq!(
                        app.durable
                            .channels
                            .iter()
                            .find(|channel| channel.id == target)
                            .unwrap()
                            .title,
                        title
                    );
                }
                let next_sequence = model.sequence + 1;
                model.fold(next_sequence, edge.as_ref().unwrap());
                drop(app.apply_workspace_model_update(ModelUpdate::Read(model)));
                assert_eq!(
                    app.workspace.order().collect::<Vec<_>>(),
                    vec![primary, card]
                );
                assert_eq!(app.workspace.focused(), Some(primary));
            }
            assert!(app.card_state(card).unwrap().existence.is_open());
            assert!(app.chat_start_pending(card));
            assert!(
                app.workspace_model
                    .views
                    .iter()
                    .find(|view| view.scope == scope)
                    .unwrap()
                    .panes
                    .iter()
                    .any(|pane| pane.id == card)
            );
            if let Some(primary) = primary {
                // A genuinely newer complete view remains authoritative even
                // when the original Insert receipt is delivered again.
                let mut removed = app.workspace_model.clone();
                removed.sequence += 1;
                let view = removed
                    .views
                    .iter_mut()
                    .find(|view| view.scope == scope)
                    .unwrap();
                view.apply(&Change::Remove { id: card }).unwrap();
                view.revision += 1;
                drop(app.apply_workspace_model_update(ModelUpdate::Read(removed.clone())));
                drop(app.apply_workspace_model_update(ModelUpdate::ViewApplied {
                    scope: scope.clone(),
                    operation: Some(*operation),
                    model: accepted_snapshot,
                }));
                assert_eq!(app.workspace.order().collect::<Vec<_>>(), vec![primary]);
                assert!(!app.card_state(card).unwrap().existence.is_open());
                assert_eq!(app.workspace_model.views, removed.views);
            }
        }
    }

    #[test]
    fn late_view_snapshot_preserves_observed_channel_and_project_removal() {
        use crate::workspace_model::views::{Scope, View};
        for close_project in [false, true] {
            let (mut app, _, repository) = app_with_project();
            let channel = ChannelId::new();
            let mut original = registry(vec![shared(channel, &repository, "Original", 0)]);
            original.views.push(View::empty(Scope::Channel(channel)));
            assert!(app.install_workspace_model_snapshot(original.clone()));
            let edit = if close_project {
                ModelEdit::CloseProject {
                    repository: repository.clone(),
                }
            } else {
                ModelEdit::DeleteChannel { id: channel }
            };
            let next_sequence = original.sequence + 2;
            drop(app.apply_workspace_model_update(ModelUpdate::Edited {
                sequence: next_sequence,
                edit,
            }));
            let metadata = app.workspace_model.clone();
            original.sequence += 1;
            original.views[0].revision += 1;
            assert!(app.install_workspace_model_snapshot(original.clone()));
            assert_eq!(app.workspace_model.sequence, next_sequence);
            assert_eq!(app.workspace_model.channels, metadata.channels);
            assert_eq!(app.workspace_model.projects, metadata.projects);
            assert_eq!(
                app.workspace_model.closed_projects,
                metadata.closed_projects
            );
            assert_eq!(app.workspace_model.views, original.views);
        }
    }

    #[test]
    fn headless_execution_adopts_exact_target_without_stealing_focus_or_resurrecting_tombstones() {
        use crate::agent::Provider;
        use crate::agent_execution::{Agent, HostedSession, Origin, Spec, Surface, Target};
        let (mut app, project, repository) = app_with_project();
        let active = ChannelId::new();
        let target = ChannelId::new();
        let focus = app.workspace.focused();
        let durable = Arc::make_mut(&mut app.durable);
        durable.channels.push(local(active, project, "Active", 0));
        durable
            .channels
            .push(local(target, project, "Execution", 1));
        durable.active_channel = Some(active);
        let session = crate::chat::Session {
            native_continuity: Default::default(),
            id: TerminalId::new(),
            proof: Some(crate::model::HostSessionProof {
                host_session_id: crate::model::HostSessionId::new("execution-host"),
                boot_id: crate::model::BootId::new("execution-boot"),
                host_incarnation: crate::model::HostIncarnation::new("execution-incarnation"),
            }),
            terminal: None,
            owned_codex_listener: None,
            launch: crate::chat::Launch {
                provider: Provider::Claude,
                session_id: None,
                recipe: LaunchRecipe {
                    program: "claude".into(),
                    arguments: Vec::new(),
                    working_directory: PathBuf::from("/repo/worktree"),
                    environment: Default::default(),
                    unset_environment: Default::default(),
                },
            },
        };
        let mut agent = Agent {
            name: None,
            id: crate::model::AgentId::new(),
            origin: Origin::User,
            spec: Spec {
                provider: Provider::Claude,
                account: None,
                target: Target {
                    repository,
                    directory: PathBuf::from("/repo/worktree"),
                    placement: Some(crate::agent_execution::Placement::Channel(target)),
                },
                surface: Surface::Chat,
                model: None,
                permission_mode: None,
                reasoning_effort: None,
            },
            placement: crate::agent_execution::Placement::Channel(target),
            session: Some(HostedSession::Chat(session.clone())),
            console: None,
            created_ms: 0,
            closed: false,
        };
        let card = CardId::from_uuid(agent.id.as_uuid());
        let mut scheduled = agent.clone();
        scheduled.origin = Origin::Automation {
            id: uuid::Uuid::new_v4(),
        };
        app.workspace_model = registry(vec![
            shared(active, &agent.spec.target.repository, "Active", 0),
            shared(target, &agent.spec.target.repository, "Execution", 1),
        ]);
        app.workspace_model.agents.push(scheduled);
        drop(app.reconcile_workspace_model());
        assert!(
            !app.durable
                .cards
                .iter()
                .any(|candidate| candidate.id == card),
            "model refresh must not mount scheduled history or its live actor"
        );
        assert!(app.adopt_execution_agent(&agent).is_some());
        assert!(
            app.chat_start_pending(card),
            "even an unnamed accepted conversation in a hidden channel attaches to its proven actor"
        );
        assert!(matches!(app.cards.get(&card), Some(CardRuntime::Loading)));
        assert_eq!(app.durable.active_channel, Some(active));
        assert_eq!(app.workspace.focused(), focus);
        assert_eq!(
            app.durable
                .channels
                .iter()
                .find(|channel| channel.id == target)
                .unwrap()
                .cards,
            vec![card]
        );
        assert_eq!(
            app.durable.card_scopes.get(&card),
            Some(&PathBuf::from("/repo/worktree"))
        );
        assert!(
            matches!(&app.durable.cards.iter().find(|entry| entry.id == card).unwrap().kind, CardKind::Chat(actual) if actual == &session)
        );
        assert!(app.adopt_execution_agent(&agent).is_none());
        Arc::make_mut(&mut app.durable)
            .cards
            .iter_mut()
            .find(|entry| entry.id == card)
            .unwrap()
            .existence = Existence::Closed {
            tombstone: crate::model::Tombstone {
                operation_id: None,
                closed_at_unix_ms: 1,
                reason: Some("user closed".into()),
            },
        };
        assert!(app.adopt_execution_agent(&agent).is_none());
        assert_eq!(
            app.durable
                .cards
                .iter()
                .filter(|entry| entry.id == card)
                .count(),
            1
        );
        agent.id = crate::model::AgentId::new();
        agent.spec.surface = Surface::Terminal;
        assert!(
            app.adopt_execution_agent(&agent).is_none(),
            "wait for the exact paired console descriptor"
        );
    }

    fn managed_display_fixture() -> (
        Cagents,
        crate::agent_execution::Agent,
        crate::workspace_model::views::Pane,
        ChannelId,
    ) {
        use crate::agent_execution::{
            Agent, HostedSession, Origin, Placement, Spec, Surface, Target,
        };
        let (mut app, project, repository) = app_with_project();
        let original = ChannelId::new();
        let destination = ChannelId::new();
        let durable = Arc::make_mut(&mut app.durable);
        durable
            .channels
            .push(local(original, project, "Original", 0));
        durable
            .channels
            .push(local(destination, project, "Current", 1));
        let CardKind::Terminal(mut console) = super::super::tests::terminal_card_state().kind
        else {
            unreachable!()
        };
        console.launch.working_directory = "/repo".into();
        console.continuity = TerminalContinuity::Live {
            host_session_id: crate::model::HostSessionId::new("managed-display"),
            boot_id: crate::model::BootId::new("managed-display"),
            host_incarnation: crate::model::HostIncarnation::new("managed-display"),
        };
        let placement = Placement::Channel(original);
        let agent = Agent {
            id: crate::model::AgentId::new(),
            name: None,
            origin: Origin::User,
            spec: Spec {
                provider: crate::agent::Provider::Claude,
                account: None,
                target: Target {
                    repository: repository.clone(),
                    directory: "/repo".into(),
                    placement: Some(placement.clone()),
                },
                surface: Surface::Terminal,
                model: None,
                permission_mode: None,
                reasoning_effort: None,
            },
            placement,
            session: Some(HostedSession::Terminal(console.clone())),
            console: Some(console.clone()),
            created_ms: 0,
            closed: false,
        };
        let pane = crate::workspace_model::views::Pane {
            id: CardId::from_uuid(agent.id.as_uuid()),
            title: "Current managed title".into(),
            user_title: None,
            pending_terminal_start: false,
            width_fraction: 0.6,
            kind: CardKind::Terminal(console),
            scope: Some("/repo".into()),
        };
        app.workspace_model = registry(vec![
            shared(original, &repository, "Original", 0),
            shared(destination, &repository, "Current", 1),
        ]);
        let mut source = crate::workspace_model::views::View::empty(original);
        source.retired.push(pane.id);
        app.workspace_model.views = vec![
            source,
            crate::workspace_model::views::View {
                scope: Placement::Channel(destination),
                revision: 9,
                panes: vec![pane.clone()],
                retired: vec![],
            },
        ];
        app.workspace_model.agents.push(agent.clone());
        (app, agent, pane, destination)
    }

    #[test]
    fn attention_unpublished_managed_adoption_requires_complete_source_proof() {
        let (mut app, mut agent, _, _) = managed_display_fixture();
        app.workspace_model.views.clear();
        let crate::agent_execution::HostedSession::Terminal(source) =
            agent.session.as_mut().unwrap()
        else {
            panic!("terminal fixture");
        };
        source.continuity = TerminalContinuity::Live {
            host_session_id: crate::model::HostSessionId::new(""),
            boot_id: crate::model::BootId::new(""),
            host_incarnation: crate::model::HostIncarnation::new(""),
        };
        agent.console.as_mut().unwrap().continuity = source.continuity.clone();
        app.workspace_model.agents = vec![agent.clone()];
        let original = app.durable.clone();
        let count = app.cards.len();
        assert!(app.adopt_execution_agent(&agent).is_none());
        assert!(Arc::ptr_eq(&original, &app.durable));
        assert_eq!(app.cards.len(), count);
        assert!(
            app.card_state(CardId::from_uuid(agent.id.as_uuid()))
                .is_none()
        );
    }

    #[test]
    fn attention_first_model_sync_adopts_current_managed_membership_before_stale_placement() {
        for worktree in [false, true] {
            let (mut app, agent, pane, destination) = managed_display_fixture();
            let original = agent.channel().unwrap();
            if worktree {
                app.workspace_model.views[1].scope = crate::agent_execution::Placement::Worktree {
                    repository: agent.spec.target.repository.clone(),
                    path: "/repo".into(),
                };
            }
            let incoming = std::mem::take(&mut app.workspace_model);
            drop(app.apply_workspace_model_update(ModelUpdate::Connected(incoming)));
            let target = app
                .card_state(pane.id)
                .expect("first incoming snapshot adopted its exact managed pane");
            assert_eq!(target.kind, pane.kind);
            assert_eq!(target.title, pane.title);
            assert!(
                app.durable
                    .channels
                    .iter()
                    .find(|channel| channel.id == original)
                    .unwrap()
                    .cards
                    .is_empty()
            );
            if worktree {
                assert!(
                    !app.durable
                        .channels
                        .iter()
                        .any(|channel| channel.cards.contains(&pane.id))
                );
                assert_eq!(
                    app.durable.card_scopes.get(&pane.id),
                    Some(&PathBuf::from("/repo"))
                );
            } else {
                assert!(
                    app.durable
                        .channels
                        .iter()
                        .find(|channel| channel.id == destination)
                        .unwrap()
                        .cards
                        .contains(&pane.id)
                );
            }
            assert_eq!(
                app.workspace_model.agents[0], agent,
                "display adoption preserves immutable launch and execution ownership"
            );
        }
    }

    #[test]
    fn attention_first_model_sync_relocates_existing_exact_managed_runtime_without_rehosting() {
        let (mut app, agent, pane, destination) = managed_display_fixture();
        let mut original_only = app.workspace_model.clone();
        original_only.views.clear();
        let display =
            crate::workspace_model::views::managed_display(&original_only, &agent).unwrap();
        assert!(app.adopt_execution_agent_display(&agent, display).is_some());
        let runtime = app.cards.remove(&pane.id).unwrap();
        app.cards.insert(pane.id, runtime);
        let actor = app.card_state(pane.id).unwrap().kind.clone();
        drop(app.reconcile_workspace_model());
        assert_eq!(app.card_state(pane.id).unwrap().kind, actor);
        assert!(matches!(
            app.cards.get(&pane.id),
            Some(CardRuntime::Loading)
        ));
        assert!(
            app.durable
                .channels
                .iter()
                .find(|channel| channel.id == destination)
                .unwrap()
                .cards
                .contains(&pane.id)
        );
        assert!(
            !app.durable
                .channels
                .iter()
                .find(|channel| channel.id == agent.channel().unwrap())
                .unwrap()
                .cards
                .contains(&pane.id)
        );
    }

    #[test]
    fn observed_managed_terminal_exit_projects_without_live_adoption_or_relaunch() {
        for relaunch_available in [false, true] {
            let (mut app, agent, mut pane, destination) = managed_display_fixture();
            let CardKind::Terminal(session) = &mut pane.kind else {
                unreachable!()
            };
            let exit = crate::model::TerminalExit {
                code: Some(1),
                signal: None,
                occurred_at_unix_ms: 1,
            };
            session.continuity = if relaunch_available {
                TerminalContinuity::RelaunchAvailable {
                    recipe: session.launch.clone(),
                    last_exit: Some(exit),
                }
            } else {
                TerminalContinuity::Exited { last_exit: exit }
            };
            app.workspace_model.views[1].panes[0] = pane.clone();
            assert!(
                crate::workspace_model::views::managed_display(&app.workspace_model, &agent)
                    .is_none(),
                "an ended terminal grants no live adoption authority"
            );
            let incoming = std::mem::take(&mut app.workspace_model);
            drop(app.apply_workspace_model_update(ModelUpdate::Connected(incoming.clone())));
            // An ended pane cannot use the eager live-agent adoption path.
            // Its passive shared projection waits for the publisher's seed
            // acknowledgements, just as an ordinary saved terminal does.
            for view in &incoming.views {
                drop(app.apply_workspace_model_update(ModelUpdate::ViewApplied {
                    scope: view.scope.clone(),
                    operation: None,
                    model: incoming.clone(),
                }));
            }
            assert!(
                app.shared_views.blocked.is_empty(),
                "a normal console exit must not block its channel"
            );
            assert_eq!(app.card_state(pane.id).unwrap().kind, pane.kind);
            assert!(matches!(
                app.cards.get(&pane.id),
                Some(CardRuntime::Completed)
            ));
            assert!(
                app.durable
                    .channels
                    .iter()
                    .find(|channel| channel.id == destination)
                    .unwrap()
                    .cards
                    .contains(&pane.id)
            );
            assert_eq!(app.workspace_model.agents[0], agent);
        }
    }

    #[test]
    fn ended_managed_terminal_cannot_substitute_another_launch_recipe() {
        for changed in ["launch", "restart"] {
            let (mut app, _, _, _) = managed_display_fixture();
            let CardKind::Terminal(session) = &mut app.workspace_model.views[1].panes[0].kind
            else {
                unreachable!()
            };
            let mut recipe = session.launch.clone();
            if changed == "launch" {
                session.launch.working_directory = "/replacement".into();
                recipe = session.launch.clone();
            } else {
                recipe.program = "replacement".into();
            }
            session.continuity = TerminalContinuity::RelaunchAvailable {
                recipe,
                last_exit: Some(crate::model::TerminalExit {
                    code: Some(0),
                    signal: None,
                    occurred_at_unix_ms: 1,
                }),
            };
            let incoming = std::mem::take(&mut app.workspace_model);
            drop(app.apply_workspace_model_update(ModelUpdate::Connected(incoming)));
            assert!(!app.shared_views.blocked.is_empty());
            assert!(
                app.durable
                    .channels
                    .iter()
                    .all(|channel| channel.cards.is_empty())
            );
        }
    }

    #[test]
    fn attention_model_sync_never_recreates_removed_replaced_or_ambiguous_managed_panes_at_old_target()
     {
        for state in ["removed", "replaced", "ambiguous"] {
            let (mut app, _, pane, _) = managed_display_fixture();
            match state {
                "removed" => app.workspace_model.views[1].panes.clear(),
                "replaced" => {
                    let CardKind::Terminal(session) =
                        &mut app.workspace_model.views[1].panes[0].kind
                    else {
                        unreachable!()
                    };
                    session.id = TerminalId::new();
                }
                _ => {
                    let mut duplicate = app.workspace_model.views[1].clone();
                    duplicate.scope = crate::agent_execution::Placement::Channel(ChannelId::new());
                    app.workspace_model.views.push(duplicate);
                }
            }
            let incoming = std::mem::take(&mut app.workspace_model);
            drop(app.apply_workspace_model_update(ModelUpdate::Connected(incoming)));
            assert!(
                app.card_state(pane.id).is_none(),
                "{state} canonical evidence cannot fall back to the original placement"
            );
            assert!(
                !app.durable
                    .channels
                    .iter()
                    .any(|channel| channel.cards.contains(&pane.id))
            );
            if state != "removed" {
                assert!(
                    !app.shared_views.blocked.is_empty(),
                    "ordinary projection must not bypass the managed replacement/ambiguity fence"
                );
            }
        }
    }

    /// A window whose project is keyed the way a real one is: by the common Git
    /// directory, which is what `ProjectState.repository` holds and therefore
    /// what the shared model keys a channel on. Keying the fixture on the work
    /// tree instead is what let a channel that no window could adopt pass every
    /// test in this module.
    fn app_with_project() -> (Cagents, ProjectId, PathBuf) {
        let mut app = super::super::tests::context_inspector_test_app(true);
        let project = ProjectId::new();
        let repository = PathBuf::from("/repo/.git");
        app.sidebar_model.projects.push(SidebarProject::new(
            project,
            RepositoryId::new(),
            "repo".into(),
            repository.clone(),
            Vec::new(),
            Vec::new(),
        ));
        (app, project, repository)
    }

    fn shared(id: ChannelId, repository: &Path, title: &str, ordinal: u32) -> ModelChannel {
        ModelChannel {
            title_source: Default::default(),
            id,
            repository: repository.to_path_buf(),
            title: title.to_owned(),
            ordinal,
        }
    }

    fn local(id: ChannelId, project: ProjectId, title: &str, ordinal: u32) -> ProjectChannel {
        ProjectChannel {
            title_source: Default::default(),
            id,
            project_id: project,
            title: title.to_owned(),
            ordinal,
            shared: false,
            cards: Vec::new(),
            focused: None,
        }
    }

    /// A registry with an identity and the channels it holds, as a window that
    /// has reconciled against it would have recorded.
    fn registry(channels: Vec<ModelChannel>) -> WorkspaceModel {
        WorkspaceModel {
            navigation_orders: Vec::new(),
            identity: uuid::Uuid::new_v4(),
            sequence: channels.len() as u64,
            projects: Vec::new(),
            channels,
            closed_projects: Vec::new(),
            views: Vec::new(),
            agents: Vec::new(),
        }
    }

    #[test]
    fn a_reconnecting_window_applies_a_missed_project_close() {
        let (mut app, project, repository) = app_with_project();
        let mut model = registry(Vec::new());
        model.closed_projects.push(repository.clone());
        drop(app.apply_workspace_model_update(ModelUpdate::Connected(model)));
        assert!(app.project_ui.closing.contains_key(&project));
        let generation = app.project_ui.close_generation[&project];
        drop(
            app.handle_project_action(super::super::project_ui::Action::ProjectSessionsClosed(
                project,
                generation,
                Ok(super::super::project_ui::ProjectSessionClosure::for_roots(
                    vec![repository.clone()],
                )),
            )),
        );
        assert!(app.sidebar_model.project(project).is_none());
        assert!(app.project_ui.closed.contains(&repository));
    }

    #[test]
    fn a_late_close_callback_cannot_override_a_reopen() {
        let (mut app, project, repository) = app_with_project();
        let mut model = registry(Vec::new());
        model.sequence = 1;
        model.closed_projects.push(repository.clone());
        drop(app.apply_workspace_model_update(ModelUpdate::Connected(model)));
        assert!(app.project_ui.closing.contains_key(&project));
        let generation = app.project_ui.close_generation[&project];

        drop(app.apply_workspace_model_update(ModelUpdate::Edited {
            sequence: 2,
            edit: ModelEdit::ReopenProject {
                kind: Default::default(),
                repository: repository.clone(),
                name: "repo".into(),
            },
        }));
        drop(app.apply_workspace_model_update(ModelUpdate::Edited {
            sequence: 1,
            edit: ModelEdit::CloseProject {
                repository: repository.clone(),
            },
        }));
        drop(
            app.handle_project_action(super::super::project_ui::Action::ProjectSessionsClosed(
                project,
                generation,
                Ok(super::super::project_ui::ProjectSessionClosure::for_roots(
                    vec![repository.clone()],
                )),
            )),
        );
        assert!(app.sidebar_model.project(project).is_some());
        assert!(!app.project_ui.closed.contains(&repository));
    }

    #[test]
    fn an_unchanged_explicit_open_acknowledges_the_pending_reopen() {
        let (mut app, project, repository) = app_with_project();
        app.project_ui.reopening.insert(project, repository.clone());
        let mut model = registry(Vec::new());
        model.projects.push(ModelProject {
            kind: Default::default(),
            repository: repository.clone(),
            name: "repo".into(),
            ordinal: 1,
        });
        drop(app.apply_workspace_model_update(ModelUpdate::Reopened {
            repository: repository.clone(),
            model,
        }));
        assert!(!app.project_ui.is_reopening(&repository));
        assert!(app.sidebar_model.project(project).is_some());
    }

    /// Replaces the publish queue with one the test can read, which is how the
    /// window's side of a publish is observed without a daemon.
    fn intercept_publishes(app: &mut Cagents) -> async_channel::Receiver<ModelEdit> {
        let (tx, rx) = async_channel::unbounded();
        app.workspace_model_tx = tx;
        rx
    }

    fn drain(rx: &async_channel::Receiver<ModelEdit>) -> Vec<ModelEdit> {
        std::iter::from_fn(|| rx.try_recv().ok()).collect()
    }

    /// A real checkout, because adopting a project runs the repository loader
    /// for real: what is being asserted is what that load does to this window.
    ///
    /// `None` when there is no usable `git`, so the suite skips rather than
    /// failing on a machine without one.
    fn git_checkout() -> Option<tempfile::TempDir> {
        let checkout = tempfile::tempdir().expect("temporary directory");
        let initialized = std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(checkout.path())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        initialized
            .is_ok_and(|status| status.success())
            .then_some(checkout)
    }

    /// Runs a task to its first window message.
    ///
    /// The repository load is the whole subject here - the window does nothing
    /// observable until it finishes - so the test drives it rather than
    /// synthesising the message it produces, which is the one thing that would
    /// hide the flag under test.
    fn first_message(task: Task<Message>) -> Option<Message> {
        use cosmic::iced::futures::StreamExt;

        let mut stream = cosmic::iced::runtime::task::into_stream(task)?;
        // A tokio runtime rather than a bare executor: the loader runs on
        // `spawn_blocking`, which panics outside one.
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a tokio runtime")
            .block_on(async move {
                while let Some(action) = stream.next().await {
                    if let cosmic::iced::runtime::Action::Output(cosmic::Action::App(message)) =
                        action
                    {
                        return Some(message);
                    }
                }
                None
            })
    }

    #[test]
    fn shared_folder_reopen_loads_new_rows_and_preserves_acknowledged_terminal_membership() {
        use crate::agent::{Activity, ActivityState, Provider};
        use crate::workspace_model::views::{Change, Scope, View};

        fn sidebar_frame(
            app: &Cagents,
            renderer: &mut cosmic::Renderer,
            navigation: &crate::sidebar::NavigationCache,
            cache: cosmic::iced::runtime::user_interface::Cache,
        ) -> (
            cosmic::iced::runtime::user_interface::Cache,
            Vec<(String, [f64; 4])>,
        ) {
            use cosmic::iced::{
                Size,
                advanced::{clipboard, mouse},
            };
            let shortcuts = crate::settings::KeyboardShortcuts::default();
            let mut ui = cosmic::iced::runtime::UserInterface::build(
                crate::sidebar::view_cached(
                    &app.sidebar_model,
                    &app.sidebar_state,
                    crate::sidebar::SidebarMode::Expanded,
                    280.0,
                    false,
                    navigation,
                    &shortcuts,
                ),
                Size::new(280.0, 900.0),
                cache,
                renderer,
            );
            let mut messages = Vec::new();
            ui.update(
                &[],
                mouse::Cursor::Unavailable,
                renderer,
                &mut clipboard::Null,
                &mut messages,
            );
            assert!(messages.is_empty());
            ui.draw(
                renderer,
                &cosmic::theme::active(),
                &cosmic::iced::advanced::renderer::Style::default(),
                mouse::Cursor::Unavailable,
            );
            let nodes = ui.a11y_nodes(mouse::Cursor::Unavailable);
            let rows = nodes
                .root()
                .iter()
                .chain(nodes.children())
                .filter_map(|node| {
                    let label = node.node().label()?;
                    let bounds = node.node().bounds()?;
                    (node.node().role() == iced_accessibility::accesskit::Role::Button
                        && !node.node().is_hidden()
                        && !node.node().is_disabled()
                        && label.starts_with("Open project "))
                    .then(|| {
                        (
                            label.to_owned(),
                            [bounds.x0, bounds.y0, bounds.x1, bounds.y1],
                        )
                    })
                })
                .collect();
            (ui.into_cache(), rows)
        }

        fn reopen(app: &mut Cagents, path: &Path, sequence: u64) {
            let task = app.update(Message::WorkspaceModelObserved(ModelUpdate::Edited {
                sequence,
                edit: ModelEdit::ReopenProject {
                    repository: path.to_path_buf(),
                    name: path.file_name().unwrap().to_string_lossy().into_owned(),
                    kind: ProjectKind::Folder,
                },
            }));
            let loaded = first_message(task).expect("the actual model edit schedules its loader");
            assert!(matches!(&loaded, Message::RepositoryLoaded {
                open_cards: false,
                result: Ok(loaded),
                ..
            } if loaded.project.repository == path && loaded.project.kind == ProjectKind::Folder));
            drop(app.update(loaded));
        }

        for explicitly_filed in [false, true] {
            let mut renderer = super::super::tests::headless_renderer();
            let navigation = crate::sidebar::NavigationCache::default();
            let directory = tempfile::tempdir().unwrap();
            let parent = directory.path().canonicalize().unwrap().join("parent");
            let nested = parent.join("nested");
            let control = directory.path().join("control");
            std::fs::create_dir_all(&nested).unwrap();
            std::fs::create_dir(&control).unwrap();
            let mut app = super::super::tests::context_inspector_test_app(true);
            let control_card = app.workspace.focused().unwrap();
            let CardKind::Terminal(mut session) =
                app.card_state(control_card).unwrap().kind.clone()
            else {
                panic!("the original control pane is a terminal")
            };
            if let CardKind::Terminal(control_session) =
                &mut app.card_state_mut(control_card).unwrap().kind
            {
                control_session.launch.working_directory = control.clone();
            }
            drop(app.select_worktree(control.clone(), false));
            app.refresh_ui.focused = false;
            let focused = app.workspace.focused();
            let strip = app.workspace.order().collect::<Vec<_>>();
            let published = intercept_publishes(&mut app);
            drop(
                app.update(Message::WorkspaceModelObserved(ModelUpdate::Connected(
                    registry(Vec::new()),
                ))),
            );
            reopen(&mut app, &parent, 1);
            assert!(app.known_worktree_roots.contains(&parent));
            assert_eq!(app.worktree_scope_for(&nested), Some(parent.clone()));
            let parent_key = app.sidebar_model.worktree_at(&parent).unwrap();
            let scope = app.worktree_view_scope(&parent).unwrap();
            assert!(matches!(&scope, Scope::Worktree { path, .. } if *path == parent));

            // A real seed acknowledgment precedes the newly observed session.
            // Unlike the old NO_REGISTRY fixture, subsequent publication and
            // canonical projection therefore both run through the App reducer.
            let mut model = app.workspace_model.clone();
            model.sequence = 2;
            model.views.push(View::empty(scope.clone()));
            drop(
                app.update(Message::WorkspaceModelObserved(ModelUpdate::ViewApplied {
                    scope: scope.clone(),
                    operation: None,
                    model,
                })),
            );
            drain(&published);
            session.id = TerminalId::new();
            session.launch.working_directory = nested.clone();
            session.continuity = TerminalContinuity::Live {
                host_session_id: crate::model::HostSessionId::new("shared-folder-test"),
                boot_id: crate::model::BootId::new("shared-folder-test"),
                host_incarnation: crate::model::HostIncarnation::new("shared-folder-test"),
            };
            let terminal = session.id;
            let mut descriptor = crate::terminal::ipc::TerminalDescriptor::relaunchable(
                terminal,
                session.launch.clone(),
                None,
            );
            descriptor.session = session.clone();
            assert!(!app.durable.cards.iter().any(|card| {
                matches!(&card.kind, CardKind::Terminal(current) if current.id == terminal)
            }));
            drop(app.update(Message::HostedTerminalsObserved(vec![descriptor])));
            let cards = app.durable.cards.iter().filter(|card| {
                matches!(&card.kind, CardKind::Terminal(current) if current.id == terminal)
            }).map(|card| card.id).collect::<Vec<_>>();
            assert_eq!(cards.len(), 1);
            let card = cards[0];
            assert!(
                !app.durable.card_scopes.contains_key(&card),
                "the known Folder boundary must prevent active-control fallback filing"
            );
            if explicitly_filed {
                Arc::make_mut(&mut app.durable)
                    .card_scopes
                    .insert(card, parent.clone());
            }
            let working = Activity {
                provider: Provider::Omp,
                state: ActivityState::Working,
            };
            app.sidebar_state.set_agent_activity(
                card,
                parent_key.project_id,
                parent_key.worktree_id,
                working,
            );
            app.sidebar_state.observe_agent_update(
                card,
                parent_key.project_id,
                parent_key.worktree_id,
            );
            app.publish_view_changes();
            let inserts = drain(&published)
                .into_iter()
                .filter_map(|edit| match edit {
                    ModelEdit::ChangeView {
                        scope: actual,
                        operation,
                        expected_revision,
                        change: change @ Change::Insert { .. },
                        ..
                    } if actual == scope => Some((operation, expected_revision, change)),
                    _ => None,
                })
                .filter(
                    |(_, _, change)| matches!(change, Change::Insert { pane, .. } if pane.id == card),
                )
                .collect::<Vec<_>>();
            assert_eq!(
                inserts.len(),
                1,
                "the real publish queue must contain the exact new pane"
            );
            let (operation, expected_revision, change) = inserts.into_iter().next().unwrap();
            let mut acknowledged = app.workspace_model.clone();
            acknowledged.sequence = 3;
            let view = acknowledged
                .views
                .iter_mut()
                .find(|view| view.scope == scope)
                .unwrap();
            assert_eq!(
                expected_revision, view.revision,
                "the captured real request must target this exact simulated CAS basis"
            );
            assert!(view.apply(&change).unwrap());
            view.revision += 1;
            drop(
                app.update(Message::WorkspaceModelObserved(ModelUpdate::ViewApplied {
                    scope,
                    operation: Some(operation),
                    model: acknowledged,
                })),
            );
            assert_eq!(
                app.durable.card_scopes.get(&card),
                Some(&parent),
                "daemon-accepted membership is authoritative in both pre-ACK cases"
            );
            let (retained_cache, before_rows) = sidebar_frame(
                &app,
                &mut renderer,
                &navigation,
                cosmic::iced::runtime::user_interface::Cache::default(),
            );
            assert!(
                before_rows
                    .iter()
                    .any(|(label, _)| label == "Open project parent, agent working")
            );
            assert!(
                !before_rows
                    .iter()
                    .any(|(label, _)| label == "Open project nested")
            );
            reopen(&mut app, &nested, 4);
            let nested_key = app
                .sidebar_model
                .worktree_at(&nested)
                .expect("the model-driven nested Folder must have its own row");
            assert_ne!(nested_key.project_id, parent_key.project_id);
            assert_eq!(
                app.sidebar_model
                    .projects
                    .iter()
                    .filter(|project| project.repository == parent)
                    .count(),
                1
            );
            assert_eq!(
                app.sidebar_model
                    .projects
                    .iter()
                    .filter(|project| project.repository == nested)
                    .count(),
                1
            );
            let (_, after_rows) = sidebar_frame(&app, &mut renderer, &navigation, retained_cache);
            for expected in ["Open project parent, agent working", "Open project nested"] {
                let matches = after_rows
                    .iter()
                    .filter(|(label, _)| label == expected)
                    .collect::<Vec<_>>();
                assert_eq!(
                    matches.len(),
                    1,
                    "the retained real sidebar must expose {expected}"
                );
                let bounds = matches[0].1;
                assert!(
                    bounds[0] >= 0.0
                        && bounds[1] >= 0.0
                        && bounds[2] > bounds[0]
                        && bounds[3] > bounds[1]
                        && bounds[2] <= 280.0
                        && bounds[3] <= 900.0
                );
            }
            assert_eq!(
                app.card_worktree(app.card_state(card).unwrap()),
                Some(parent.as_path())
            );
            assert_eq!(
                app.sidebar_state
                    .agent_activity(parent_key.project_id, parent_key.worktree_id),
                Some(ActivityState::Working)
            );
            assert!(
                app.sidebar_state
                    .has_agent_update(parent_key.project_id, parent_key.worktree_id)
            );
            assert_eq!(
                app.sidebar_state
                    .agent_activity(nested_key.project_id, nested_key.worktree_id),
                None
            );
            assert!(
                !app.sidebar_state
                    .has_agent_update(nested_key.project_id, nested_key.worktree_id)
            );
            assert_eq!(app.durable.card_scopes.get(&card), Some(&parent));
            assert_eq!(app.active_workspace.as_ref(), Some(&control));
            assert_eq!(app.workspace.focused(), focused);
            assert_eq!(app.workspace.order().collect::<Vec<_>>(), strip);
            assert!(
                matches!(&app.card_state(card).unwrap().kind, CardKind::Terminal(current) if *current == session)
            );
        }
    }

    /// A project this window has never seen - opened from a shell with
    /// `cagents project open`, or in another window - arrives as a sidebar row
    /// and nothing else.
    ///
    /// Adoption used to read the repository the way a user opening one in this
    /// window does, which selects its worktree and starts a terminal in it. In
    /// every window at once: the user's active worktree was replaced under
    /// them by a repository someone else named, and a terminal they never
    /// started appeared in it. A project the registry names is news, not an
    /// instruction.
    #[test]
    fn a_fresh_window_adopts_the_shared_folder_kind_without_git_rediscovery() {
        let directory = tempfile::tempdir().unwrap();
        let repository = directory.path().canonicalize().unwrap();
        std::fs::create_dir(repository.join(".git")).unwrap();
        let mut app = super::super::tests::context_inspector_test_app(false);
        app.workspace_model = WorkspaceModel {
            projects: vec![ModelProject {
                repository: repository.clone(),
                name: "Folder".into(),
                kind: ProjectKind::Folder,
                ordinal: 1,
            }],
            ..registry(Vec::new())
        };
        let message = first_message(app.reconcile_workspace_model()).unwrap();
        drop(app.update(message));
        let project = app.sidebar_model.projects.first().unwrap();
        assert_eq!(project.repository, repository);
        assert_eq!(project.kind, ProjectKind::Folder);
        assert!(project.worktrees.is_empty());
        assert!(app.durable.cards.is_empty());
    }

    #[test]
    fn an_adopted_project_is_a_sidebar_row_and_takes_neither_a_card_nor_the_place() {
        let Some(checkout) = git_checkout() else {
            eprintln!("skipping: `git` is not available on PATH");
            return;
        };
        let (mut app, _, _) = app_with_project();
        let cards = app.durable.cards.len();
        let focused = app.workspace.focused();
        // The key the CLI and a window both derive: the common Git directory.
        let repository =
            canonical_repository(&checkout.path().join(".git")).expect("an absolute key");
        app.workspace_model = WorkspaceModel {
            projects: vec![ModelProject {
                kind: Default::default(),
                repository: repository.clone(),
                name: "opened elsewhere".into(),
                ordinal: 1,
            }],
            ..registry(Vec::new())
        };

        let loaded = first_message(app.reconcile_workspace_model())
            .expect("adopting a project reads the repository it names");
        let _ = app.update(loaded);

        assert!(
            app.sidebar_model
                .projects
                .iter()
                .any(|project| project.repository == repository),
            "the adopted project is in this window's sidebar"
        );
        assert_eq!(
            app.durable.cards.len(),
            cards,
            "adoption starts no terminal, so no card appears"
        );
        assert_eq!(
            app.active_workspace, None,
            "and it does not move the user into the repository it just heard about"
        );
        assert_eq!(app.workspace.focused(), focused);
    }

    /// A channel another window created arrives as a scope, not as a
    /// conversation: N windows adopting one channel must not start N chats in
    /// it, and the cards inside a channel are each window's own business.
    ///
    /// It also has to reach the sidebar. The key the channel carries is the
    /// project's common Git directory, and when the CLI keyed it on the work
    /// tree instead, adoption found no owner and retained the channel forever:
    /// the model held it, the store never did, and no window showed it.
    #[test]
    fn an_adopted_channel_arrives_empty_and_starts_no_conversation() {
        let (mut app, project, repository) = app_with_project();
        let cards = app.durable.cards.len();
        let id = ChannelId::new();
        app.workspace_model = registry(vec![shared(id, &repository, "created elsewhere", 7)]);

        let _ = app.reconcile_workspace_model();

        let adopted = app
            .durable
            .channels
            .iter()
            .find(|channel| channel.id == id)
            .expect("the channel is adopted into this window");
        assert_eq!(adopted.project_id, project);
        assert_eq!(adopted.title, "created elsewhere");
        assert_eq!(adopted.ordinal, 7);
        assert!(
            adopted.cards.is_empty(),
            "an adopted channel holds no cards"
        );
        assert_eq!(adopted.focused, None);
        assert_eq!(
            app.durable.cards.len(),
            cards,
            "adoption starts no conversation, so no card appears"
        );
        app.refresh_sidebar_channels();
        let rows = crate::sidebar::channels_in_display_order(
            app.sidebar_model.projects[0].channels.iter(),
        );
        assert_eq!(
            rows.iter().map(|row| row.id).collect::<Vec<_>>(),
            vec![id],
            "an adopted channel is a row, not just a record"
        );
    }

    /// The registry's rank decides the order, so an adopted channel created
    /// after a local one renders above it - in every window, whichever one made
    /// which.
    #[test]
    fn a_newer_adopted_channel_renders_above_an_older_local_one() {
        let (mut app, project, repository) = app_with_project();
        let mine = ChannelId::new();
        Arc::make_mut(&mut app.durable)
            .channels
            .push(local(mine, project, "mine", 3));
        let adopted = ChannelId::new();
        app.workspace_model = registry(vec![
            shared(mine, &repository, "mine", 3),
            shared(adopted, &repository, "theirs", 9),
        ]);

        let _ = app.reconcile_workspace_model();
        app.refresh_sidebar_channels();

        let rows = crate::sidebar::channels_in_display_order(
            app.sidebar_model.projects[0].channels.iter(),
        );
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].id, adopted, "the newer channel is on top");
        assert_eq!(rows[1].id, mine);
    }

    /// A channel deleted anywhere disappears everywhere, and its cards are
    /// detached locally after the initiating client has retired host work.
    #[test]
    fn a_deleted_channel_detaches_its_cards_without_a_second_host_close() {
        let (mut app, project, repository) = app_with_project();
        let id = ChannelId::new();
        let conversation = app.durable.cards[0].id;
        let mut channel = local(id, project, "doomed", 4);
        channel.cards = vec![conversation];
        channel.focused = Some(conversation);
        Arc::make_mut(&mut app.durable).channels.push(channel);
        app.workspace_model = registry(vec![shared(id, &repository, "doomed", 4)]);
        let published = intercept_publishes(&mut app);

        let _ = app.apply_workspace_model_update(ModelUpdate::Edited {
            sequence: 5,
            edit: ModelEdit::DeleteChannel { id },
        });
        assert!(app.channel_ui.was_deleted(id));
        assert!(!app.channel_ui.is_deleting(id));
        assert!(!drain(&published).iter().any(|edit| matches!(edit, ModelEdit::CreateChannel { id: revived, .. } if *revived == id)), "an in-flight deletion must not resurrect an unacknowledged channel");

        assert!(
            !app.durable.channels.iter().any(|channel| channel.id == id),
            "the channel is gone from this window too"
        );
        let card = app
            .durable
            .cards
            .iter()
            .find(|card| card.id == conversation)
            .expect("the card is tombstoned rather than forgotten");
        assert!(
            !card.existence.is_open(),
            "a conversation that lived only in the channel goes with it"
        );
    }

    /// A window is not sent the echo of its own deletion, so its cached model
    /// still names the channel afterwards, and the next reconciliation - any
    /// other window's edit, a repository finishing loading, or a read taken
    /// before the registry applied the deletion - adopted it straight back as
    /// an empty channel.
    #[test]
    fn a_channel_this_window_deleted_is_never_adopted_back() {
        let (mut app, project, repository) = app_with_project();
        let id = ChannelId::new();
        let mut channel = local(id, project, "doomed", 4);
        channel.shared = true;
        Arc::make_mut(&mut app.durable).channels.push(channel);
        let model = registry(vec![shared(id, &repository, "doomed", 4)]);
        app.workspace_model = model.clone();
        let published = intercept_publishes(&mut app);

        let _ = app.delete_channel(id);
        assert!(!app.durable.channels.iter().any(|channel| channel.id == id));
        assert!(drain(&published).iter().any(
            |edit| matches!(edit, ModelEdit::DeleteChannel { id: deleted } if *deleted == id)
        ));

        let _ = app.reconcile_workspace_model();
        let _ = app.apply_workspace_model_update(ModelUpdate::Read(model));
        assert!(
            !app.durable.channels.iter().any(|channel| channel.id == id),
            "a model that predates the deletion does not revive the channel"
        );
        assert!(drain(&published).is_empty());
    }

    /// Before a window has shared a channel, the registry's silence about that
    /// channel carries no information at all. Retiring on it would tombstone the
    /// cards inside a channel nobody deleted, so the channel is published
    /// instead - which is also the whole migration for a workspace that predates
    /// the registry.
    #[test]
    fn a_window_joining_with_an_unshared_channel_keeps_and_publishes_it() {
        let (mut app, project, repository) = app_with_project();
        let id = ChannelId::new();
        let conversation = app.durable.cards[0].id;
        let mut channel = local(id, project, "mine", 0);
        channel.title_source = crate::model::ChannelTitleSource::Prompt;
        channel.cards = vec![conversation];
        channel.focused = Some(conversation);
        Arc::make_mut(&mut app.durable).channels.push(channel);
        let published = intercept_publishes(&mut app);

        let _ = app.apply_workspace_model_update(ModelUpdate::Connected(registry(Vec::new())));

        let kept = app
            .durable
            .channels
            .iter()
            .find(|channel| channel.id == id)
            .expect("a registry that never held a channel cannot retire it");
        assert_eq!(
            kept.cards,
            vec![conversation],
            "the cards inside it are untouched"
        );
        assert!(
            app.durable
                .cards
                .iter()
                .find(|card| card.id == conversation)
                .is_some_and(|card| card.existence.is_open()),
            "nothing was tombstoned"
        );
        let edits = drain(&published);
        assert!(edits.iter().any(|edit| matches!(edit,ModelEdit::SeedView{scope,panes,..} if *scope==crate::workspace_model::views::Scope::Channel(id) && panes.iter().any(|pane|pane.id==conversation))));
        assert_eq!(
            edits
                .into_iter()
                .filter(|edit| !matches!(edit, ModelEdit::SeedView { .. }))
                .collect::<Vec<_>>(),
            vec![
                ModelEdit::OpenProject {
                    kind: Default::default(),
                    repository: repository.clone(),
                    name: "repo".into(),
                },
                ModelEdit::CreateChannel {
                    id,
                    repository,
                    // The title the user has already seen, not an empty one: a
                    // channel being introduced is not a channel being created.
                    title: "mine".into(),
                    title_source: crate::model::ChannelTitleSource::Prompt,
                },
            ],
            "the project comes first, or the channel would have no owner"
        );
    }

    /// Once this window has recorded that the registry holds a channel, the
    /// registry's silence about it does mean something: it was deleted somewhere
    /// else, and this window honours that through its own deletion path.
    #[test]
    fn a_window_joining_after_a_shared_channel_was_deleted_elsewhere_retires_it() {
        let (mut app, project, _repository) = app_with_project();
        let id = ChannelId::new();
        let conversation = app.durable.cards[0].id;
        let identity = uuid::Uuid::new_v4();
        let mut channel = local(id, project, "shared then deleted", 4);
        channel.shared = true;
        channel.cards = vec![conversation];
        channel.focused = Some(conversation);
        let durable = Arc::make_mut(&mut app.durable);
        durable.channels.push(channel);
        durable.workspace_model_identity = Some(identity);
        let published = intercept_publishes(&mut app);

        let _ = app.apply_workspace_model_update(ModelUpdate::Connected(WorkspaceModel {
            identity,
            ..registry(Vec::new())
        }));

        let _ = app.handle_channel_ui(super::super::channel_ui::Action::SessionsClosed(
            id,
            uuid::Uuid::new_v4(),
            Ok(()),
        ));

        assert!(
            !app.durable.channels.iter().any(|channel| channel.id == id),
            "a channel this window shared and the registry no longer has was \
             deleted elsewhere"
        );
        assert!(
            app.durable
                .cards
                .iter()
                .find(|card| card.id == conversation)
                .is_some_and(|card| !card.existence.is_open()),
            "it retires through the local deletion path, which tombstones its \
             cards rather than erasing them"
        );
        // The retirement goes through `delete_channel`, which publishes the
        // deletion right back - a no-op the registry answers cheaply, and the
        // price of one deletion path that behaves identically whoever asked.
        // What must never happen is a create: that would argue with a deletion
        // and resurrect the channel in every other window.
        assert!(
            !drain(&published)
                .iter()
                .any(|edit| matches!(edit, ModelEdit::CreateChannel { .. })),
            "a deletion is honoured, never argued with by republishing"
        );
    }

    /// A shared flag is a claim about one registry. A replaced daemon or a wiped
    /// data directory is a different registry, so every flag is stale and
    /// nothing may be retired on its authority.
    #[test]
    fn a_replaced_registry_retires_nothing_and_is_told_what_this_window_has() {
        let (mut app, project, _repository) = app_with_project();
        let id = ChannelId::new();
        let mut channel = local(id, project, "mine", 4);
        channel.shared = true;
        let durable = Arc::make_mut(&mut app.durable);
        durable.channels.push(channel);
        durable.workspace_model_identity = Some(uuid::Uuid::new_v4());
        let published = intercept_publishes(&mut app);

        let replacement = registry(Vec::new());
        let identity = replacement.identity;
        let _ = app.apply_workspace_model_update(ModelUpdate::Connected(replacement));

        let kept = app
            .durable
            .channels
            .iter()
            .find(|channel| channel.id == id)
            .expect("a registry this window never shared with cannot retire it");
        assert!(
            !kept.shared,
            "the flag was a claim about the registry that is gone"
        );
        assert_eq!(app.durable.workspace_model_identity, Some(identity));
        assert!(
            drain(&published)
                .iter()
                .any(|edit| matches!(edit, ModelEdit::CreateChannel { id: published, .. } if *published == id)),
            "the replacement registry is told about it instead"
        );
    }

    /// The rank the registry assigns replaces the provisional one a local
    /// creation started with, so the window that created a channel and the
    /// window that adopted it agree on where it sorts. The same answer is the
    /// proof the registry holds it.
    #[test]
    fn the_registry_rank_replaces_the_provisional_one_and_records_the_sharing() {
        let (mut app, project, _repository) = app_with_project();
        let id = ChannelId::new();
        let provisional = app.next_provisional_ordinal();
        Arc::make_mut(&mut app.durable)
            .channels
            .push(local(id, project, "", provisional));
        assert_ne!(provisional, 11);

        let _ = app.apply_workspace_model_update(ModelUpdate::Ranked {
            channel: id,
            ordinal: 11,
        });

        let ranked = app
            .durable
            .channels
            .iter()
            .find(|channel| channel.id == id)
            .unwrap();
        assert_eq!(ranked.ordinal, 11);
        assert!(
            ranked.shared,
            "the registry answered about it, so it holds it"
        );
    }

    /// A window reconciles on its own events too - a repository finishing
    /// loading is one - and those can happen before any daemon has answered.
    /// The cached model is empty then, and that emptiness is the window's own,
    /// not a registry's: acting on it would publish prematurely and, for a
    /// channel a previous session had shared, retire it against nobody's word.
    #[test]
    fn a_window_that_has_reached_no_registry_yet_neither_retires_nor_publishes() {
        let (mut app, project, _repository) = app_with_project();
        let id = ChannelId::new();
        let mut channel = local(id, project, "mine", 4);
        channel.shared = true;
        Arc::make_mut(&mut app.durable).channels.push(channel);
        let published = intercept_publishes(&mut app);
        assert_eq!(app.workspace_model.identity, NO_REGISTRY);

        let _ = app.reconcile_workspace_model();

        assert!(
            app.durable.channels.iter().any(|channel| channel.id == id),
            "no registry has spoken, so nothing it did not say can be acted on"
        );
        assert!(drain(&published).is_empty());
    }
}
