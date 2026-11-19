//! Mirrors the daemon's shared sidebar model into this window's own store.
//!
//! The daemon owns shared membership and descriptors; this window retains its
//! local presentation and pending intentions. One thread watches revisioned
//! changes and another publishes this window's mutations. Neither runs on the UI
//! thread, because a redraw must not wait on a daemon that may not be running.

use super::*;
use crate::control::{ControlClient, ControlError, ModelEdge};
use crate::model::{ChannelId, ProjectChannel};
use crate::workspace_model::{
    ModelChannel, ModelEdit, ModelProject, NO_REGISTRY, PENDING_ORDINAL, WorkspaceModel,
    canonical_repository,
    delta::{EditEffects, EditReceipt, Mirror, ViewChanges},
};

/// How long a thread waits before reconnecting.
///
/// A window can open before any daemon exists, and the daemon can be replaced
/// under it. Neither is an error, so the threads retry rather than giving up -
/// and slowly, because a reconnection re-reads the whole model and therefore
/// cannot miss anything by being late.
const RECONNECT_DELAY: Duration = Duration::from_secs(2);

/// How long after its first send a pane change may still be resent. The daemon
/// remembers an applied operation for a week (`OPERATION_RETENTION`) so a
/// resend whose reply was lost is recognized as done; a change older than this
/// is never resent, so it can never outlive that memory and apply twice, even
/// across a week-long suspend or a clock a few days off. The window reconciles
/// from the daemon's model instead, as for any refused change.
const REPLAY_LIMIT: Duration = Duration::from_secs(24 * 60 * 60);

/// An edit taken from the window, and when it was first sent.
type Unsent = (ModelRequest, std::time::SystemTime);

/// Local publication identity never crosses the daemon protocol. A scope may
/// be seeded again before an earlier receipt reaches the UI.
#[derive(Debug, Clone)]
pub(super) struct ModelRequest {
    pub(super) edit: ModelEdit,
    pub(super) seed_attempt: Option<uuid::Uuid>,
}

impl From<ModelEdit> for ModelRequest {
    fn from(edit: ModelEdit) -> Self {
        let seed_attempt = matches!(&edit, ModelEdit::SeedView { .. }).then(uuid::Uuid::new_v4);
        Self { edit, seed_attempt }
    }
}

/// Whether `edit`, first sent at `first`, may still be sent `now`. A clock
/// that moved backwards past the first send proves nothing, so it may not.
fn may_resend(edit: &ModelEdit, first: std::time::SystemTime, now: std::time::SystemTime) -> bool {
    !matches!(edit, ModelEdit::ChangeView { .. })
        || now
            .duration_since(first)
            .is_ok_and(|waited| waited <= REPLAY_LIMIT)
}

/// What the daemon threads hand the window.
#[derive(Debug, Clone)]
pub enum ModelUpdate {
    /// Spawn metadata arrives before the model that first exposes its agent.
    AgentLabels(Vec<(crate::model::AgentId, String)>),
    /// The registry as it stood when the watcher connected.
    ///
    /// This is the only update that means "a connection just began", which is
    /// why it is distinct from [`ModelUpdate::Read`]: it is where the window
    /// compares the registry's identity against the one it recorded and decides
    /// whether anything it holds has ever been shared with this registry.
    Connected(WorkspaceModel),
    ConnectedClosed {
        model: WorkspaceModel,
        closed: Vec<CardId>,
    },
    /// The publisher waits until the observer establishes this registry before
    /// draining edits. Its reconnect snapshot remains private to its mirror.
    PublisherConnected {
        identity: uuid::Uuid,
        ready: std::sync::mpsc::SyncSender<bool>,
    },
    /// One local write, including canonical rows and only its affected scopes.
    Committed {
        receipt: EditReceipt,
        edit: ModelEdit,
        views: ViewChanges,
    },
    /// Refresh only the scope whose immutable source fields conflicted.
    ScopeConflicted {
        identity: Option<uuid::Uuid>,
        scope: crate::workspace_model::views::Scope,
        operation: uuid::Uuid,
        change: crate::workspace_model::views::Change,
        views: ViewChanges,
        error: String,
    },
    RestoreCanonicalTitle,
    /// The daemon answered but has no shared model to give, so this window's
    /// shared panes have no registry to wait for.
    Unavailable(String),
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
    /// An edge that changed views, or channel or agent rows beyond what its
    /// edit says, with what it changed: complete views for the scopes it
    /// changed, rather than the whole model.
    Changed {
        sequence: u64,
        edit: ModelEdit,
        changes: EdgeChanges,
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
    /// The registry's answer to this window's request to retire an emptied
    /// channel: the model after it decided, either way. A channel still in it
    /// was kept.
    RetirementAnswered {
        channel: ChannelId,
        model: WorkspaceModel,
    },
    /// A definitive refusal of the edit itself, rather than an unknown
    /// transport outcome that the publisher will retry.
    RetirementFailed {
        channel: ChannelId,
        error: String,
    },
    /// Optional naming yielded after temporary admission pressure. The next
    /// real wake may retry this exact current candidate; this is not a timer.
    TitleDeferred(ModelEdit),
    PaneTransferFailed {
        operation: uuid::Uuid,
        error: String,
    },
    /// A seed receipt follows the delta path only while this local offer owns
    /// the scope. The attempt is never serialized to the daemon.
    SeedCommitted {
        attempt: uuid::Uuid,
        receipt: EditReceipt,
        edit: ModelEdit,
        views: ViewChanges,
    },
    SeedApplied {
        scope: crate::workspace_model::views::Scope,
        attempt: uuid::Uuid,
        model: WorkspaceModel,
    },
    SeedRefused {
        scope: crate::workspace_model::views::Scope,
        attempt: uuid::Uuid,
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
        model: WorkspaceModel,
    },
}

/// What one edge changed, ready to install over the window's model.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EdgeChanges {
    pub identity: Option<uuid::Uuid>,
    pub views: ViewChanges,
    pub effects: EditEffects,
}

/// Whether `model` is a registry's and holds this project row as a window
/// would open it.
///
/// `OpenProject` is an upsert of the name and kind: against a row the registry
/// holds identically it commits nothing, and its receipt changes nothing
/// either. Every repository load publishes the project it opened, so a window
/// restoring a workspace would otherwise send one such edit per project and
/// reconcile each receipt. A tombstoned or unknown project, a renamed one, or
/// one whose registry has not answered yet still publishes: there the edit (or
/// its refusal) is the news.
fn holds_project(model: &WorkspaceModel, repository: &Path, name: &str, kind: ProjectKind) -> bool {
    model.identity != NO_REGISTRY
        && model
            .project(repository)
            .is_some_and(|held| held.name == name && held.kind == kind)
}

/// A title edge may use the metadata-only projector only when its canonical
/// effects contain no other work. Scoped recovery or lifecycle effects still
/// need the complete reconciliation, even when the original edit was a title.
fn channel_title_only(
    edit: &ModelEdit,
    effects: &EditEffects,
    views: &ViewChanges,
) -> Option<ChannelId> {
    let (ModelEdit::RetitleChannel { id, .. } | ModelEdit::SuggestChannelTitle { id, .. }) = edit
    else {
        return None;
    };
    (effects.views.is_empty()
        && effects.closed.is_empty()
        && effects.projects.is_empty()
        && effects.removed_projects.is_empty()
        && effects.removed_channels.is_empty()
        && effects.navigation_orders.is_empty()
        && effects.agents.is_empty()
        && effects.removed_agents.is_empty()
        && effects.channels.iter().all(|channel| channel.id == *id)
        && views.views.is_empty()
        && views.removed.is_empty()
        && views.absent.is_empty())
    .then_some(*id)
}

/// Metadata rows have independent clocks: a newer receipt for one scope must
/// not discard a delayed observer edge for a different channel or project.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) enum MetadataKey {
    Project(PathBuf),
    Channel(ChannelId),
    Navigation(PathBuf),
    Agent(crate::model::AgentId),
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
    observe_with_client(sender, &mut client)
}

fn observe_with_client(
    sender: &async_channel::Sender<ModelUpdate>,
    client: &mut ControlClient,
) -> Result<(), String> {
    // One request subscribes and reads, so an edit applied between the two
    // cannot fall into a gap - the hazard `watch_terminals` then `list` has to
    // order around with two requests.
    let (model, closed) = match client.watch_workspace_state() {
        Ok(state) => state,
        Err(ControlError::Remote { message, .. })
            if message == crate::workspace_model::REGISTRY_UNAVAILABLE =>
        {
            sender
                .send_blocking(ModelUpdate::Unavailable(message.clone()))
                .map_err(|error| error.to_string())?;
            return Err(message);
        }
        Err(error) => return Err(error.to_string()),
    };
    let mut mirror = Mirror::for_model(&model);
    let identity = model.identity;
    let mut labelled = HashSet::new();
    read_new_agent_labels(client, sender, identity, &model.agents, &mut labelled)?;
    sender
        .send_blocking(ModelUpdate::ConnectedClosed { model, closed })
        .map_err(|error| error.to_string())?;
    loop {
        let ModelEdge {
            sequence,
            edit,
            effects,
        } = client
            .next_workspace_model_edit()
            .map_err(|error| error.to_string())?;
        read_new_agent_labels(client, sender, identity, &effects.agents, &mut labelled)?;
        let applied = mirror.apply(&edit, &effects);
        let mut views = applied.changes;
        views.sequence = sequence;
        for scope in applied.unresolved {
            let (view, read_sequence) = client
                .read_workspace_view(scope.clone())
                .map_err(|error| error.to_string())?;
            let installed = mirror.install_at(scope, view, read_sequence);
            views.observed.extend(installed.observed);
            views.views.extend(installed.views);
            views.removed.extend(installed.removed);
            views.absent.extend(installed.absent);
        }
        sender
            .send_blocking(ModelUpdate::Changed {
                sequence,
                edit,
                changes: EdgeChanges {
                    identity: Some(identity),
                    views,
                    effects,
                },
            })
            .map_err(|error| error.to_string())?;
    }
}

/// Labels are immutable acceptance metadata. Read them once per new inventory,
/// off the UI thread, rather than loading a hidden transcript or polling it.
fn read_new_agent_labels(
    client: &mut ControlClient,
    sender: &async_channel::Sender<ModelUpdate>,
    registry: uuid::Uuid,
    agents: &[crate::agent_execution::Agent],
    labelled: &mut HashSet<crate::model::AgentId>,
) -> Result<(), String> {
    if agents.iter().all(|agent| labelled.contains(&agent.id)) {
        return Ok(());
    }
    let (identity, labels) = client
        .read_agent_labels()
        .map_err(|error| error.to_string())?;
    if identity != registry {
        return Err("The workspace registry changed while reading agent labels".into());
    }
    labelled.extend(labels.iter().map(|(id, _)| *id));
    sender
        .send_blocking(ModelUpdate::AgentLabels(labels))
        .map_err(|error| error.to_string())
}

/// Publishes this window's shared-model mutations.
pub(super) fn publish_workspace_model(
    updates: async_channel::Sender<ModelUpdate>,
    requests: async_channel::Receiver<ModelRequest>,
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
    requests: &async_channel::Receiver<ModelRequest>,
    pending: &mut Option<Unsent>,
) -> Result<(), String> {
    // Unlike the observer, the publisher starts a daemon: a channel the user
    // just created has to reach the registry, and there is no registry without
    // a daemon to hold it.
    let mut client = crate::terminal::connect_ready_daemon()
        .map_err(|error| error.to_string())?
        .client;
    // This connection may reach a replaced registry. Its one baseline is a
    // reconnect read; subsequent edits and conflicts read affected scopes only.
    let (model, _) = client
        .read_workspace_state()
        .map_err(|error| error.to_string())?;
    let mut mirror = Mirror::for_model(&model);
    let (ready, waiting) = std::sync::mpsc::sync_channel(1);
    updates
        .send_blocking(ModelUpdate::PublisherConnected {
            identity: model.identity,
            ready,
        })
        .map_err(|error| error.to_string())?;
    // The observer owns registry identity. Until the UI has received its
    // baseline, keep every queued edit and any uncertain prior write pending.
    // Only this worker waits; the UI and daemon hold no lock for the handshake.
    loop {
        match waiting.recv_timeout(Duration::from_millis(100)) {
            Ok(true) => break,
            Ok(false) => {
                return Err("the publisher is waiting for the observer's registry baseline".into());
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return Err("the window stopped publishing".into());
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) if updates.is_closed() => {
                return Err("the window stopped publishing".into());
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
    publish_with_mirror(updates, requests, pending, &mut client, &mut mirror)
}

#[cfg(test)]
fn publish_with_client(
    updates: &async_channel::Sender<ModelUpdate>,
    requests: &async_channel::Receiver<ModelRequest>,
    pending: &mut Option<Unsent>,
    client: &mut ControlClient,
) -> Result<(), String> {
    publish_with_mirror(updates, requests, pending, client, &mut Mirror::default())
}

fn publish_with_mirror(
    updates: &async_channel::Sender<ModelUpdate>,
    requests: &async_channel::Receiver<ModelRequest>,
    pending: &mut Option<Unsent>,
    client: &mut ControlClient,
    mirror: &mut Mirror,
) -> Result<(), String> {
    loop {
        let (request, first) = match pending.take() {
            Some((request, first)) => {
                if !may_resend(&request.edit, first, std::time::SystemTime::now()) {
                    tracing::warn!(
                        "a pane change waited too long for the daemon and was not resent"
                    );
                    if let ModelEdit::ChangeView {
                        scope, operation, ..
                    } = &request.edit
                    {
                        let _ = updates.send_blocking(ModelUpdate::ViewRefused {
                            scope: scope.clone(),
                            operation: *operation,
                            error: "The pane change waited too long for the daemon to be resent"
                                .into(),
                        });
                    }
                    continue;
                }
                (request, first)
            }
            None => (
                requests
                    .recv_blocking()
                    .map_err(|_| "the window stopped publishing".to_owned())?,
                std::time::SystemTime::now(),
            ),
        };
        // Retained until it is sent, so a daemon replaced mid-publish costs a
        // reconnection rather than the edit.
        *pending = Some((request.clone(), first));
        let seed_attempt = request.seed_attempt;
        let request = request.edit;
        match send_with_mirror(client, request.clone(), seed_attempt, updates, mirror) {
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
                    if matches!(&request, ModelEdit::SuggestConversationTitle { .. }) {
                        // A refused suggestion is not replayed against a newer
                        // owner. Restore the authoritative name after optimism.
                        let _ = updates.send_blocking(ModelUpdate::RestoreCanonicalTitle);
                    }
                    if let ModelEdit::ChangeView {
                        scope, operation, ..
                    } = &request
                    {
                        if let ModelEdit::ChangeView { change, .. } = &request
                            && matches!(
                                change,
                                crate::workspace_model::views::Change::RemoveSource { .. }
                                    | crate::workspace_model::views::Change::UpdateTerminalSource { .. }
                            )
                        {
                            let (view, sequence) = client
                                .read_workspace_view(scope.clone())
                                .map_err(|error| error.to_string())?;
                            let views = mirror.install_at(scope.clone(), view, sequence);
                            let _ = updates.send_blocking(ModelUpdate::ScopeConflicted {
                                identity: mirror.identity(),
                                scope: scope.clone(),
                                operation: *operation,
                                change: change.clone(),
                                views,
                                error: error.to_string(),
                            });
                        } else {
                            let _ = updates.send_blocking(ModelUpdate::ViewRefused {
                                scope: scope.clone(),
                                operation: *operation,
                                error: error.to_string(),
                            });
                        }
                    }
                    if let (
                        ModelEdit::SeedView { scope, .. },
                        ControlError::Remote { message, .. },
                    ) = (&request, &error)
                    {
                        let _ = updates.send_blocking(ModelUpdate::SeedRefused {
                            scope: scope.clone(),
                            attempt: seed_attempt.expect("a seed publication has an attempt"),
                            error: message.clone(),
                        });
                    }
                    if let ModelEdit::RetireEmptyChannel { home, .. } = &request {
                        let _ = updates.send_blocking(ModelUpdate::RetirementFailed {
                            channel: home.channel,
                            error: error.to_string(),
                        });
                    }
                    if let ModelEdit::ReopenProject { repository, .. } = request {
                        let _ = updates.send_blocking(ModelUpdate::ReopenFailed {
                            repository,
                            error: error.to_string(),
                        });
                    }
                    // The daemon answered definitively on this connection.
                    // Keep draining independent edits; only an unknown outcome
                    // needs reconnection and its backoff.
                    continue;
                }
                return Err(error.to_string());
            }
        }
    }
}

#[cfg(test)]
fn send(
    client: &mut ControlClient,
    edit: ModelEdit,
    seed_attempt: Option<uuid::Uuid>,
    updates: &async_channel::Sender<ModelUpdate>,
) -> Result<(), ControlError> {
    send_with_mirror(client, edit, seed_attempt, updates, &mut Mirror::default())
}

fn send_with_mirror(
    client: &mut ControlClient,
    edit: ModelEdit,
    seed_attempt: Option<uuid::Uuid>,
    updates: &async_channel::Sender<ModelUpdate>,
    mirror: &mut Mirror,
) -> Result<(), ControlError> {
    use crate::workspace_model::views::{Change, Scope};
    let mut request = edit;
    let mut conflicts = 0;
    let mut title_busy = 0;
    let receipt = loop {
        match client.apply_workspace_edit(request.clone()) {
            Ok(receipt) => break receipt,
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
                let _ = updates.send_blocking(ModelUpdate::RestoreCanonicalTitle);
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
                let ModelEdit::ChangeView {
                    scope,
                    operation,
                    change,
                    expected_revision,
                } = &mut request
                else {
                    return Err(error);
                };
                let (view, read_sequence) = client.read_workspace_view(scope.clone())?;
                mirror.install_at(scope.clone(), view.clone(), read_sequence);
                let scoped = WorkspaceModel {
                    views: view.clone().into_iter().collect(),
                    ..Default::default()
                };
                let removal_changed = match change {
                    Change::RemoveSource { before, .. } => !view.as_ref().is_some_and(|view| {
                        let mut sources = view.panes.iter().filter(|pane| pane.id == before.id);
                        sources.next().is_some_and(|pane| {
                            crate::workspace_model::views::same_removal_source(before, pane)
                        }) && sources.next().is_none()
                    }),
                    _ => false,
                };
                if removal_changed || source_update_conflicts(&scoped, scope, change) {
                    let views = mirror.install_at(scope.clone(), view, read_sequence);
                    let _ = updates.send_blocking(ModelUpdate::ScopeConflicted {
                        identity: mirror.identity(),
                        scope: scope.clone(),
                        operation: *operation,
                        change: change.clone(),
                        views,
                        error: error.to_string(),
                    });
                    return Ok(());
                }
                *expected_revision = view.as_ref().map_or(0, |view| view.revision);
            }
            Err(error) => return Err(error),
        }
    };
    let applied = mirror.apply(&request, &receipt.effects);
    let mut views = applied.changes;
    views.sequence = receipt.sequence;
    let mut scopes: Vec<Scope> = applied.unresolved;
    // An operation replay may have no new effects. Its acknowledgment still
    // settles the exact pending intent against current scoped authority.
    if receipt.effects.views.is_empty() {
        match &request {
            ModelEdit::SeedView { scope, .. } => scopes.push(scope.clone()),
            ModelEdit::ChangeView { scope, change, .. } => {
                scopes.push(scope.clone());
                if let Change::Transfer { destination, .. } = change
                    && destination != scope
                {
                    scopes.push(destination.clone());
                }
            }
            _ => {}
        }
    }
    for scope in scopes {
        let (view, read_sequence) = client.read_workspace_view(scope.clone())?;
        let installed = mirror.install_at(scope, view, read_sequence);
        views.observed.extend(installed.observed);
        views.views.extend(installed.views);
        views.removed.extend(installed.removed);
        views.absent.extend(installed.absent);
    }
    let update = if let Some(attempt) = seed_attempt {
        ModelUpdate::SeedCommitted {
            attempt,
            receipt,
            edit: request,
            views,
        }
    } else {
        ModelUpdate::Committed {
            receipt,
            edit: request,
            views,
        }
    };
    let _ = updates.send_blocking(update);
    Ok(())
}

pub(in crate::app) fn source_update_conflicts(
    model: &WorkspaceModel,
    scope: &crate::workspace_model::views::Scope,
    change: &crate::workspace_model::views::Change,
) -> bool {
    use crate::workspace_model::views::{Change, updates_source};
    let (Change::Update { before, pane } | Change::UpdateTerminalSource { before, pane, .. }) =
        change
    else {
        return false;
    };
    match model.views.iter().find(|view| &view.scope == scope) {
        Some(view) => view.source_update_conflicts(before, pane),
        None => updates_source(before, pane),
    }
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

/// Absence candidates only; the caller retains deletion/republish policy.
/// Shared IDs include rows skipped during adoption. Local order and duplicate
/// entries remain authoritative for the later action loop.
fn channels_absent_from_model(
    local: &[ProjectChannel],
    shared: &[ModelChannel],
) -> Vec<(ChannelId, bool)> {
    let shared_ids = shared
        .iter()
        .map(|channel| channel.id)
        .collect::<HashSet<_>>();
    local
        .iter()
        .filter(|channel| !shared_ids.contains(&channel.id))
        .map(|channel| (channel.id, channel.shared))
        .collect()
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

    fn newer_live_ancestors(&self, sequence: u64) -> (HashSet<ChannelId>, HashSet<PathBuf>) {
        let mut protected_channels = HashSet::new();
        let mut protected_projects = HashSet::new();
        for (scope, observed) in &self.shared_views.view_sequences {
            if *observed <= sequence
                || !self
                    .workspace_model
                    .views
                    .iter()
                    .any(|view| &view.scope == scope)
            {
                continue;
            }
            match scope {
                crate::workspace_model::views::Scope::Channel(id) => {
                    protected_channels.insert(*id);
                    if let Some(channel) = self.workspace_model.channel(*id) {
                        protected_projects.insert(channel.repository.clone());
                    }
                }
                crate::workspace_model::views::Scope::Worktree { repository, .. } => {
                    protected_projects.insert(repository.clone());
                }
            }
        }
        for (key, observed) in &self.shared_views.metadata_revisions {
            if *observed <= sequence {
                continue;
            }
            match key {
                MetadataKey::Channel(id) => {
                    if let Some(channel) = self.workspace_model.channel(*id) {
                        protected_projects.insert(channel.repository.clone());
                    }
                }
                MetadataKey::Navigation(repository) => {
                    if self
                        .workspace_model
                        .navigation_orders
                        .iter()
                        .any(|order| &order.repository == repository)
                    {
                        protected_projects.insert(repository.clone());
                    }
                }
                _ => {}
            }
        }
        (protected_channels, protected_projects)
    }

    /// Keep complete view/agent snapshots ordered independently of observed
    /// metadata edges. A title/rank/project edge has no replacement pane data.
    pub(super) fn install_workspace_model_snapshot(&mut self, mut model: WorkspaceModel) -> bool {
        let snapshot_sequence = model.sequence;
        if model.identity != self.workspace_model.identity {
            self.shared_views.snapshot_sequence = 0;
            self.shared_views.metadata_revisions.clear();
            self.shared_views.view_sequences.clear();
            self.shared_views.scope_absences.clear();
            self.shared_views.closed.clear();
        } else if model.sequence < self.shared_views.snapshot_sequence {
            return false;
        } else if model.sequence < self.workspace_model.sequence
            || self
                .shared_views
                .view_sequences
                .values()
                .any(|sequence| *sequence > model.sequence)
            || self
                .shared_views
                .scope_absences
                .values()
                .any(|sequence| *sequence > model.sequence)
        {
            // A reconnect baseline can race a newer publisher receipt or a
            // scoped read taken ahead of the last processed model edge. Keep
            // each newer row while still installing unrelated changes this
            // complete read recovered during the lost connection.
            let sequence = model.sequence;
            let (protected_channels, protected_projects) = self.newer_live_ancestors(sequence);
            let newer = |key: &MetadataKey| {
                self.shared_views
                    .metadata_revisions
                    .get(key)
                    .is_some_and(|held| *held > sequence)
            };
            // A newer child proves its ancestors exist, not that their cached
            // metadata is newer. Only each row's own clock can replace a row
            // present in the reconnect baseline.
            for project in &self.workspace_model.projects {
                if newer(&MetadataKey::Project(project.repository.clone()))
                    || (protected_projects.contains(&project.repository)
                        && model.project(&project.repository).is_none())
                {
                    model
                        .projects
                        .retain(|held| held.repository != project.repository);
                    model.projects.push(project.clone());
                }
            }
            model.projects.retain(|project| {
                !newer(&MetadataKey::Project(project.repository.clone()))
                    || self.workspace_model.project(&project.repository).is_some()
            });
            for path in &self.workspace_model.closed_projects {
                if newer(&MetadataKey::Project(path.clone()))
                    && !model.closed_projects.contains(path)
                {
                    model.closed_projects.push(path.clone());
                }
            }
            model.closed_projects.retain(|path| {
                !protected_projects.contains(path)
                    && (!newer(&MetadataKey::Project(path.clone()))
                        || self.workspace_model.closed_projects.contains(path))
            });
            for channel in &self.workspace_model.channels {
                if newer(&MetadataKey::Channel(channel.id))
                    || (protected_channels.contains(&channel.id)
                        && model.channel(channel.id).is_none())
                {
                    model.channels.retain(|held| held.id != channel.id);
                    model.channels.push(channel.clone());
                }
            }
            model.channels.retain(|channel| {
                !newer(&MetadataKey::Channel(channel.id))
                    || self.workspace_model.channel(channel.id).is_some()
            });
            for agent in &self.workspace_model.agents {
                if newer(&MetadataKey::Agent(agent.id)) {
                    model.agents.retain(|held| held.id != agent.id);
                    model.agents.push(agent.clone());
                }
            }
            model.agents.retain(|agent| {
                !newer(&MetadataKey::Agent(agent.id))
                    || self
                        .workspace_model
                        .agents
                        .iter()
                        .any(|held| held.id == agent.id)
            });
            for order in &self.workspace_model.navigation_orders {
                if newer(&MetadataKey::Navigation(order.repository.clone())) {
                    model
                        .navigation_orders
                        .retain(|held| held.repository != order.repository);
                    model.navigation_orders.push(order.clone());
                }
            }
            model.navigation_orders.retain(|order| {
                !newer(&MetadataKey::Navigation(order.repository.clone()))
                    || self
                        .workspace_model
                        .navigation_orders
                        .iter()
                        .any(|held| held.repository == order.repository)
            });
            model.views.retain(|view| {
                !self
                    .shared_views
                    .scope_absences
                    .get(&view.scope)
                    .is_some_and(|absence| *absence > sequence)
            });
            for view in &self.workspace_model.views {
                let previous = model.views.iter().find(|held| held.scope == view.scope);
                let preserve = previous.is_some_and(|held| held.revision < view.revision)
                    || (previous.is_none()
                        && self
                            .shared_views
                            .view_sequences
                            .get(&view.scope)
                            .is_some_and(|held| *held > sequence));
                if preserve {
                    model.views.retain(|held| held.scope != view.scope);
                    model.views.push(view.clone());
                }
            }
            model.sequence = model.sequence.max(self.workspace_model.sequence);
        }
        // `model.sequence` may retain a newer receipt's high watermark; the
        // caller's original read sequence is the complete baseline's floor.
        self.shared_views.snapshot_sequence =
            self.shared_views.snapshot_sequence.max(snapshot_sequence);
        self.shared_views
            .metadata_revisions
            .retain(|_, sequence| *sequence > self.shared_views.snapshot_sequence);
        let open: HashSet<_> = model
            .views
            .iter()
            .flat_map(|view| view.panes.iter().map(|pane| pane.id))
            .collect();
        self.shared_views.closed.extend(
            model
                .views
                .iter()
                .flat_map(|view| view.retired.iter().copied())
                .filter(|id| !open.contains(id)),
        );
        // Fence what this snapshot fences before anything publishes against
        // it. Only projection lifts a fence, with the aliases it recomputes.
        let display_index = crate::workspace_model::views::ManagedDisplayIndex::new(&model);
        let fenced =
            super::shared_views::quarantined_managed_panes(&model, &model.views, &display_index);
        self.shared_views.quarantined.extend(fenced);
        self.workspace_model = model;
        true
    }

    pub(super) fn apply_workspace_model_update(&mut self, update: ModelUpdate) -> Task<Message> {
        self.apply_workspace_model_update_effect(update).0
    }

    /// Gives the application an explicit tail budget. Only a proven
    /// presentation change may avoid structural sidebar/ownership work.
    pub(super) fn apply_workspace_model_update_projection(
        &mut self,
        update: ModelUpdate,
    ) -> (Task<Message>, Projection) {
        // A registry receipt can arrive after this scope began another seed.
        // Fence it before installing rows or capturing any projection source.
        let update = match update {
            ModelUpdate::SeedCommitted {
                attempt,
                receipt,
                edit,
                views,
            } => {
                let ModelEdit::SeedView { scope, .. } = &edit else {
                    return (Task::none(), Projection::Unchanged);
                };
                if self.shared_views.seed_attempts.get(scope) != Some(&attempt) {
                    return (Task::none(), Projection::Unchanged);
                }
                ModelUpdate::Committed {
                    receipt,
                    edit,
                    views,
                }
            }
            update => update,
        };
        let foreign = match &update {
            ModelUpdate::Committed { receipt, .. } => {
                receipt.identity != self.workspace_model.identity
            }
            ModelUpdate::Changed { changes, .. } => changes
                .identity
                .is_some_and(|identity| identity != self.workspace_model.identity),
            _ => false,
        };
        if foreign {
            return (Task::none(), Projection::Unchanged);
        }
        let update = match update {
            ModelUpdate::Committed {
                receipt,
                edit,
                views,
            } if self.receipt_confirms_held_project(&edit, &receipt.effects, &views) => {
                tracing::trace!(target: "cagents::workspace_projection", selected = true, "Confirmed a held project");
                // Only the metadata clock advances: the row is the one held.
                let installed = self.install_changed_edge(
                    receipt.sequence,
                    &edit,
                    EdgeChanges {
                        identity: Some(receipt.identity),
                        views,
                        effects: receipt.effects,
                    },
                );
                return (installed.unwrap_or_else(Task::none), Projection::Unchanged);
            }
            update => update,
        };
        match self.presentation_scope_for_update(&update) {
            Ok(scope) => {
                tracing::trace!(target: "cagents::workspace_projection", selected = true, "Projecting shared presentation");
                return self.apply_presentation_model_update(update, scope);
            }
            Err(reason) => {
                tracing::trace!(target: "cagents::workspace_projection", selected = false, reason, "Projecting shared model")
            }
        }
        let (task, changed) = self.apply_workspace_model_update_full_effect(update);
        (
            task,
            if changed {
                Projection::Full
            } else {
                Projection::Unchanged
            },
        )
    }

    pub(super) fn apply_workspace_model_update_effect(
        &mut self,
        update: ModelUpdate,
    ) -> (Task<Message>, bool) {
        let (task, projection) = self.apply_workspace_model_update_projection(update);
        (task, projection != Projection::Unchanged)
    }

    /// Whether this window's own `OpenProject` receipt only confirms the
    /// project row the window already holds: no view, channel, agent or
    /// lifecycle effect, and the very row installed. Such a receipt changes
    /// nothing a reconciliation reads, and settles no intent this window keeps
    /// (a reopen does, and is a different edit), so it needs no projection.
    fn receipt_confirms_held_project(
        &self,
        edit: &ModelEdit,
        effects: &EditEffects,
        views: &ViewChanges,
    ) -> bool {
        let ModelEdit::OpenProject { repository, .. } = edit else {
            return false;
        };
        let [project] = effects.projects.as_slice() else {
            return false;
        };
        views.is_empty()
            && effects.views.is_empty()
            && effects.closed.is_empty()
            && effects.channels.is_empty()
            && effects.removed_projects.is_empty()
            && effects.removed_channels.is_empty()
            && effects.navigation_orders.is_empty()
            && effects.agents.is_empty()
            && effects.removed_agents.is_empty()
            && &project.repository == repository
            && self.workspace_model.project(repository) == Some(project)
            && !self.workspace_model.closed_projects.contains(repository)
    }

    fn presentation_scope_for_update(
        &self,
        update: &ModelUpdate,
    ) -> Result<crate::workspace_model::views::Scope, &'static str> {
        use crate::workspace_model::views::Change;
        // The active-strip projector can commit this draft through
        // apply_viewport before returning to the common projection tail.
        if self.settings_blur_pending() {
            return Err("a settings draft is leaving focus");
        }
        let (edit, effects, changes, pending, identity) = match update {
            ModelUpdate::Committed {
                receipt,
                edit,
                views,
            } => {
                let ModelEdit::ChangeView { operation, .. } = edit else {
                    return Err("not a view update");
                };
                (
                    edit,
                    &receipt.effects,
                    views,
                    Some(*operation),
                    Some(receipt.identity),
                )
            }
            ModelUpdate::Changed { edit, changes, .. } => {
                // The observer can win the race against our receipt. Its exact
                // operation may project through the held overlay, but only
                // the publisher receipt below acknowledges that intent.
                let pending = match edit {
                    ModelEdit::ChangeView {
                        operation,
                        scope,
                        change,
                        ..
                    } if self
                        .shared_views
                        .pending
                        .iter()
                        .any(|(id, owner, captured)| {
                            id == operation && owner == scope && captured == change
                        }) =>
                    {
                        Some(*operation)
                    }
                    _ => None,
                };
                (
                    edit,
                    &changes.effects,
                    &changes.views,
                    pending,
                    changes.identity,
                )
            }
            _ => return Err("not a scoped edge"),
        };
        if identity != Some(self.workspace_model.identity) {
            return Err("no matching registry authority");
        }
        let ModelEdit::ChangeView { scope, change, .. } = edit else {
            return Err("not a presentation update");
        };
        let Change::Update { before, pane } = change else {
            return Err("not a presentation update");
        };
        if before.id != pane.id
            || before.kind != pane.kind
            || before.scope != pane.scope
            || before.pending_terminal_start != pane.pending_terminal_start
        {
            return Err("requested source changed");
        }
        if !effects.closed.is_empty()
            || !effects.channels.is_empty()
            || !effects.projects.is_empty()
            || !effects.removed_channels.is_empty()
            || !effects.removed_projects.is_empty()
            || !effects.navigation_orders.is_empty()
            || !effects.agents.is_empty()
            || !effects.removed_agents.is_empty()
            || effects.views.len() > 1
            || effects
                .views
                .iter()
                .any(|revision| &revision.scope != scope || revision.after.is_none())
        {
            return Err("canonical metadata or lifecycle changed");
        }
        if changes.views.len() != 1
            || !changes.removed.is_empty()
            || !changes.absent.is_empty()
            || changes.views[0].scope != *scope
        {
            return Err("not one complete scope");
        }
        let mut held = self
            .workspace_model
            .views
            .iter()
            .filter(|view| &view.scope == scope);
        let Some(held_view) = held.next() else {
            return Err("scope is not held");
        };
        if held.next().is_some() {
            return Err("scope identity is ambiguous");
        }
        if let Some(operation) = pending
            && !self
                .shared_views
                .pending
                .iter()
                .any(|(id, owner, captured)| {
                    *id == operation && owner == scope && captured == change
                })
        {
            return Err("receipt does not acknowledge the held intent");
        }
        self.presentation_projection_ready(held_view, &changes.views[0], pending)?;
        Ok(scope.clone())
    }

    fn apply_presentation_model_update(
        &mut self,
        update: ModelUpdate,
        scope: crate::workspace_model::views::Scope,
    ) -> (Task<Message>, Projection) {
        let (sequence, edit, changes, operation) = match update {
            ModelUpdate::Committed {
                receipt,
                edit,
                views,
            } => {
                let operation = match &edit {
                    ModelEdit::ChangeView { operation, .. } => Some(*operation),
                    _ => None,
                };
                (
                    receipt.sequence,
                    edit,
                    EdgeChanges {
                        identity: Some(receipt.identity),
                        views,
                        effects: receipt.effects,
                    },
                    operation,
                )
            }
            ModelUpdate::Changed {
                sequence,
                edit,
                changes,
            } => (sequence, edit, changes, None),
            _ => unreachable!("only a proven scoped edge selects presentation projection"),
        };
        let active_primary = self.durable.active_channel.and_then(|id| {
            self.durable
                .channels
                .iter()
                .find(|channel| channel.id == id)
                .map(|channel| (id, self.channel_primary_chat(channel)))
        });
        let Some(installed) = self.install_changed_edge(sequence, &edit, changes) else {
            return (Task::none(), Projection::Unchanged);
        };
        if let Some(operation) = operation {
            self.acknowledge_view_edit(scope.clone(), Some(operation));
            self.retire_settled_empty_channels();
            self.retarget_transferred_pane_updates(None);
        }
        let projected = self.reconcile_shared_presentation(std::slice::from_ref(&scope));
        // A new view proof also wakes retained automatic names. These tails
        // inspect canonical open views and must run even for presentation-only
        // changes, just as they do after a complete reconciliation.
        let mut phase = WorkspaceProjectionTiming::new(true);
        let naming_source = Arc::downgrade(&self.durable);
        let openings = self.refresh_channel_title_suggestions();
        self.restore_channel_names();
        phase.mark("selected_naming");
        let names_unchanged = naming_source.as_ptr() == Arc::as_ptr(&self.durable);
        // Saving snapshots the active strip, even for an inactive scope.
        // Its normalized order can change which open chat inherits a manual
        // channel title. Closed-member pruning cannot change that primary.
        let active_primary_unchanged = active_primary
            == self.durable.active_channel.and_then(|id| {
                self.durable
                    .channels
                    .iter()
                    .find(|channel| channel.id == id)
                    .map(|channel| (id, self.channel_primary_chat(channel)))
            });
        let projection = if names_unchanged && active_primary_unchanged {
            Projection::ScopedChrome(self.presentation_chrome_cards(&scope))
        } else {
            // A retained name can belong to another channel; its cards need
            // the same chrome refresh as the complete projection.
            Projection::Chrome
        };
        if !matches!(projection, Projection::ScopedChrome(_))
            || !self.shared_views.adoptions.is_empty()
            || !self.shared_views.restorations.is_empty()
            || !self.shared_views.chat_adoptions.is_empty()
        {
            self.materialize_replica_snapshot();
        }
        (Task::batch([installed, projected, openings]), projection)
    }

    /// Only stale complete snapshots skip projection. Even an equal model can
    /// restore optimistic local state after refusal (for example channel titles).
    /// Newer metadata edges may still lack an older snapshot's full inventory.
    fn apply_workspace_model_update_full_effect(
        &mut self,
        update: ModelUpdate,
    ) -> (Task<Message>, bool) {
        if let ModelUpdate::RetirementAnswered { channel, model } = update {
            // The registry never echoes this window's own retirement, so the
            // answer's model is the evidence of it: a channel absent there
            // retired, even when metadata folded since keeps an older channel
            // list. It retires here while the retirement still reads as this
            // window's own, which keeps its cards reopenable.
            let retired =
                model.channel(channel).is_none() && model.identity == self.workspace_model.identity;
            let (mut task, applied) =
                self.apply_workspace_model_update_effect(ModelUpdate::Read(model));
            if retired {
                self.workspace_model
                    .channels
                    .retain(|shared| shared.id != channel);
                task = Task::batch([task, self.retire_peer_channel(channel)]);
            }
            self.channel_ui.retiring.remove(&channel);
            return (
                Task::batch([task, self.finish_channel_reopen(channel)]),
                applied || retired,
            );
        }
        if let ModelUpdate::Read(model) = &update
            && model.identity == self.workspace_model.identity
            && model.sequence < self.shared_views.snapshot_sequence
        {
            return (Task::none(), false);
        }
        // The observer repeats this on every reconnection attempt.
        if matches!(update, ModelUpdate::Unavailable(_)) && self.shared_views.registry_unavailable {
            return (Task::none(), false);
        }
        (self.apply_workspace_model_update_inner(update), true)
    }

    fn apply_workspace_model_update_inner(&mut self, update: ModelUpdate) -> Task<Message> {
        match update {
            ModelUpdate::AgentLabels(labels) => {
                self.chrome_ui.agent_labels.extend(labels);
                // An activation that waited on this metadata has a new basis
                // even when the model sequence and local panes did not change.
                self.notification_ui.scope_changed();
                let reconcile = self.reconcile_workspace_model();
                let activation = self.retry_notification_activation();
                Task::batch([reconcile, activation, self.sync_chrome()])
            }
            ModelUpdate::SeedCommitted { .. } => {
                unreachable!("seed receipts are fenced before projection")
            }
            ModelUpdate::ConnectedClosed { model, closed } => {
                if model.identity == self.workspace_model.identity
                    && model.sequence < self.shared_views.snapshot_sequence
                {
                    return Task::none();
                }
                self.shared_views.registry_unavailable = false;
                self.join_workspace_model(model.identity);
                self.install_workspace_model_snapshot(model);
                self.shared_views.closed.extend(closed);
                let closed = self.apply_closed_project_tombstones();
                let reconciled = self.reconcile_workspace_model();
                self.sweep_empty_channels();
                Task::batch([closed, reconciled])
            }
            ModelUpdate::PublisherConnected { identity, ready } => {
                let matches = identity != NO_REGISTRY && identity == self.workspace_model.identity;
                let _ = ready.send(matches);
                Task::none()
            }
            ModelUpdate::Committed {
                receipt,
                edit,
                views,
            } => {
                if receipt.identity != self.workspace_model.identity {
                    return Task::none();
                }
                let operation = match &edit {
                    ModelEdit::ChangeView { operation, .. } => Some(*operation),
                    _ => None,
                };
                let transferred =
                    operation.and_then(|id| self.shared_views.requested_transfer_card(id));
                let title = channel_title_only(&edit, &receipt.effects, &views);
                let Some(result) = self.install_changed_edge(
                    receipt.sequence,
                    &edit,
                    EdgeChanges {
                        identity: Some(receipt.identity),
                        views,
                        effects: receipt.effects,
                    },
                ) else {
                    return Task::none();
                };
                match &edit {
                    ModelEdit::ChangeView { scope, .. } => {
                        self.acknowledge_view_edit(scope.clone(), operation);
                        self.retire_settled_empty_channels();
                        self.retarget_transferred_pane_updates(transferred);
                    }
                    ModelEdit::SeedView { scope, .. } => {
                        self.acknowledge_view_edit(scope.clone(), None)
                    }
                    ModelEdit::RetireEmptyChannel { home, .. } => {
                        self.channel_ui.retiring.remove(&home.channel);
                    }
                    ModelEdit::ReopenProject { repository, .. } => {
                        self.project_ui
                            .reopening
                            .retain(|_, path| path != repository);
                    }
                    _ => {}
                }
                let finish = match edit {
                    ModelEdit::RetireEmptyChannel { home, .. } => {
                        self.finish_channel_reopen(home.channel)
                    }
                    _ => Task::none(),
                };
                let projected = title
                    .and_then(|id| self.absorb_channel_title(id))
                    .unwrap_or_else(|| self.reconcile_workspace_model());
                Task::batch([result, finish, projected])
            }
            ModelUpdate::ScopeConflicted {
                identity,
                scope,
                operation,
                change,
                views,
                error,
            } => {
                if identity.is_some_and(|identity| identity != self.workspace_model.identity) {
                    return Task::none();
                }
                self.install_edge_views(views, true);
                self.refuse_conflicting_view_change(scope, operation, change, error)
            }
            ModelUpdate::RestoreCanonicalTitle => self.reconcile_workspace_model(),
            ModelUpdate::RetirementFailed { channel, error } => {
                self.channel_ui.retiring.remove(&channel);
                self.fail_channel_reopen(
                    channel,
                    &format!("Could not reopen the channel: {error}. Try reopening again."),
                );
                Task::none()
            }
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
            ModelUpdate::SeedApplied {
                scope,
                attempt,
                model,
            } => {
                if self.shared_views.seed_attempts.get(&scope) != Some(&attempt) {
                    return Task::none();
                }
                self.apply_workspace_model_update_inner(ModelUpdate::ViewApplied {
                    scope,
                    operation: None,
                    model,
                })
            }
            ModelUpdate::SeedRefused {
                scope,
                attempt,
                error,
            } => {
                if !self.refuse_view_seed(scope.clone(), attempt, error.clone()) {
                    return Task::none();
                }
                tracing::warn!(?scope, %error, "the registry refused a view seed");
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
                    let closed = self
                        .shared_views
                        .pending
                        .iter()
                        .find(|(id, owner, _)| *id == operation && *owner == scope)
                        .and_then(|(_, _, change)| match change {
                            crate::workspace_model::views::Change::Remove { id } => Some(*id),
                            crate::workspace_model::views::Change::RemoveSource {
                                before, ..
                            } => Some(before.id),
                            _ => None,
                        });
                    let kept = closed.map(|card| self.pane_kept_notice(card));
                    self.refuse_shared_view_edit(&scope, operation);
                    // The error is the registry's own wording, or a resend's
                    // deadline; the trace keeps it, and the person is told
                    // what happened to the pane.
                    let (Some(card), Some(kept)) = (closed, kept) else {
                        self.status = Some(TransientNotice::warning(
                            super::shared_views::PANE_CHANGE_NOT_SAVED,
                        ));
                        return self.reconcile_shared_views();
                    };
                    let projected = self.reconcile_shared_views();
                    self.report_refused_close(&scope, operation, card, &error, kept);
                    return projected;
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
            ModelUpdate::Unavailable(error) => {
                tracing::debug!(%error, "this daemon has no shared workspace model");
                self.shared_views.registry_unavailable = true;
                let attach = self.attach_next_restored_agent();
                let release = self.release_held_terminals();
                // No shared panes are coming for a scope that waited on them.
                let default = self.run_deferred_default_pane();
                Task::batch([attach, release, self.attach_next_restored_agent(), default])
            }
            ModelUpdate::Connected(model) => {
                if model.identity == self.workspace_model.identity
                    && model.sequence < self.shared_views.snapshot_sequence
                {
                    return Task::none();
                }
                self.shared_views.registry_unavailable = false;
                self.join_workspace_model(model.identity);
                self.install_workspace_model_snapshot(model);
                let closed = self.apply_closed_project_tombstones();
                let reconciled = self.reconcile_workspace_model();
                self.sweep_empty_channels();
                Task::batch([closed, reconciled])
            }
            ModelUpdate::RetirementAnswered { .. } => {
                unreachable!("a retirement answer is applied as the snapshot it carries")
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
            ModelUpdate::Edited { sequence, edit } => self.apply_model_edge(sequence, edit, None),
            ModelUpdate::Changed {
                sequence,
                edit,
                changes,
            } => self.apply_changed_edge(sequence, &edit, changes),
            ModelUpdate::Ranked { channel, model } => {
                // The publisher and observer use separate connections. Keep the
                // complete response: a rank alone cannot fence an older observer
                // read, and a late create echo must not erase this rank proof.
                if model.identity == NO_REGISTRY
                    || model.identity != self.workspace_model.identity
                    || model.sequence < self.shared_views.snapshot_sequence
                    || self.channel_ui.is_deleting(channel)
                    || self.channel_ui.was_deleted(channel)
                    || !self
                        .durable
                        .channels
                        .iter()
                        .any(|local| local.id == channel)
                {
                    return Task::none();
                }
                let Some(receipt) = model
                    .channel(channel)
                    .filter(|entry| entry.ordinal != PENDING_ORDINAL)
                    .cloned()
                else {
                    return Task::none();
                };
                if model.sequence < self.workspace_model.sequence
                    && self.workspace_model.channel(channel).is_none()
                {
                    // Newer folded metadata removed this identity. The older
                    // publisher response cannot introduce it again.
                    return Task::none();
                }
                if !self.install_workspace_model_snapshot(model) {
                    return Task::none();
                }
                if let Some(current) = self
                    .workspace_model
                    .channels
                    .iter_mut()
                    .find(|entry| entry.id == channel)
                    && current.repository == receipt.repository
                    && current.ordinal == PENDING_ORDINAL
                {
                    // A newer unrelated echo can precede an older full reply.
                    // Preserve that newer metadata while completing this exact
                    // channel's still-pending rank from its canonical receipt.
                    current.ordinal = receipt.ordinal;
                }
                let closed = self.apply_closed_project_tombstones();
                Task::batch([closed, self.reconcile_workspace_model()])
            }
        }
    }

    /// Folds one observed edge, and installs what it changed when the observer
    /// resolved that from its copy of the views.
    fn apply_model_edge(
        &mut self,
        sequence: u64,
        edit: ModelEdit,
        changes: Option<EdgeChanges>,
    ) -> Task<Message> {
        if sequence <= self.workspace_model.sequence {
            // A snapshot overtook this edge, so its channel and agent rows are
            // older than the window's. A view the observer read for it can
            // still be newer than that snapshot: the edit that made it newer
            // arrives as an edge the observer then has nothing to apply for.
            // A scope the snapshot lacks was deleted after this edge; one
            // created again after the snapshot arrives with a later edge.
            let refreshed =
                changes.is_some_and(|changes| self.install_edge_views(changes.views, false));
            let reconciled = if refreshed {
                self.reconcile_workspace_model()
            } else {
                Task::none()
            };
            if let ModelEdit::DeleteChannel { id } = edit {
                // A snapshot can overtake this explicit instruction.
                // It must still revoke automatic revival, even when
                // absence already removed the local channel.
                self.forget_channel_revival(id);
                self.workspace_model
                    .channels
                    .retain(|channel| channel.id != id);
                let retired = self.retire_peer_channel(id);
                return Task::batch([reconciled, retired, self.finish_channel_reopen(id)]);
            }
            return reconciled;
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
        let keys: Vec<_> = match &edit {
            ModelEdit::OpenProject { repository, .. }
            | ModelEdit::ReopenProject { repository, .. } => {
                vec![MetadataKey::Project(repository.clone())]
            }
            ModelEdit::CloseProject { repository } => {
                std::iter::once(MetadataKey::Project(repository.clone()))
                    .chain(
                        self.workspace_model
                            .channels
                            .iter()
                            .filter(|channel| &channel.repository == repository)
                            .map(|channel| MetadataKey::Channel(channel.id)),
                    )
                    .collect()
            }
            ModelEdit::CreateChannel { id, .. }
            | ModelEdit::DeleteChannel { id }
            | ModelEdit::RetitleChannel { id, .. }
            | ModelEdit::SuggestChannelTitle { id, .. }
            | ModelEdit::SuggestConversationTitle { id, .. } => vec![MetadataKey::Channel(*id)],
            ModelEdit::ResetNavigation { repository }
            | ModelEdit::ReorderNavigation { repository, .. } => {
                vec![MetadataKey::Navigation(repository.clone())]
            }
            _ => Vec::new(),
        };
        for key in keys {
            self.shared_views.metadata_revisions.insert(key, sequence);
        }
        self.workspace_model.fold(sequence, &edit);
        if let Some(changes) = changes {
            return self.apply_changed_edge(sequence, &edit, changes);
        }
        if let ModelEdit::RetitleChannel { id, .. } | ModelEdit::SuggestChannelTitle { id, .. } =
            &edit
            && let Some(task) = self.absorb_channel_title(*id)
        {
            return task;
        }
        let removed = closing.map_or_else(Task::none, |repository| {
            self.close_shared_project(&repository)
        });
        let retired = deleted.map_or_else(Task::none, |id| {
            // An explicit deletion is final even if an automatic
            // retirement of this identity was already in flight.
            self.forget_channel_revival(id);
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
            let retired = self.retire_peer_channel(id);
            Task::batch([retired, self.finish_channel_reopen(id)])
        });
        Task::batch([removed, retired, self.reconcile_workspace_model()])
    }

    /// Installs one observed change and projects it once. A publishing window
    /// uses the same installation before acknowledging its pending operation,
    /// then performs its one projection after that overlay has been removed.
    fn apply_changed_edge(
        &mut self,
        sequence: u64,
        edit: &ModelEdit,
        changes: EdgeChanges,
    ) -> Task<Message> {
        let title = channel_title_only(edit, &changes.effects, &changes.views);
        let Some(installed) = self.install_changed_edge(sequence, edit, changes) else {
            return Task::none();
        };
        if let Some(id) = title
            && let Some(title) = self.absorb_channel_title(id)
        {
            return Task::batch([installed, title]);
        }
        Task::batch([installed, self.reconcile_workspace_model()])
    }

    /// Installs canonical rows and scope authority, preserving lifecycle tasks
    /// without projecting the workspace or advancing a complete-model watermark.
    fn install_changed_edge(
        &mut self,
        sequence: u64,
        edit: &ModelEdit,
        changes: EdgeChanges,
    ) -> Option<Task<Message>> {
        if changes
            .identity
            .is_some_and(|identity| identity != self.workspace_model.identity)
        {
            return None;
        }
        self.shared_views
            .closed
            .extend(changes.effects.closed.iter().copied());
        self.install_edge_views(
            changes.views,
            sequence > self.shared_views.snapshot_sequence,
        );
        if sequence < self.shared_views.snapshot_sequence {
            return Some(Task::none());
        }
        let floor = self.shared_views.snapshot_sequence;
        let (protected_channels, protected_projects) =
            if changes.effects.removed_projects.is_empty()
                && changes.effects.removed_channels.is_empty()
            {
                (HashSet::new(), HashSet::new())
            } else {
                self.newer_live_ancestors(sequence)
            };
        let revisions = &mut self.shared_views.metadata_revisions;
        let mut accepts = |key| {
            let held = revisions.entry(key).or_insert(floor);
            if sequence < *held {
                false
            } else {
                *held = sequence;
                true
            }
        };
        let model = &mut self.workspace_model;
        let effects = changes.effects;
        let mut closed_projects = Vec::new();
        let mut removed_channels = Vec::new();
        for project in effects.projects {
            if !accepts(MetadataKey::Project(project.repository.clone())) {
                continue;
            }
            model
                .closed_projects
                .retain(|path| path != &project.repository);
            match model
                .projects
                .iter_mut()
                .find(|held| held.repository == project.repository)
            {
                Some(held) => *held = project,
                None => model.projects.push(project),
            }
        }
        for repository in effects.removed_projects {
            // Closing a project also deletes its order. Record that tombstone
            // even if a later reopen or live child already protects the parent.
            if accepts(MetadataKey::Navigation(repository.clone())) {
                model
                    .navigation_orders
                    .retain(|held| held.repository != repository);
            }
            if protected_projects.contains(&repository)
                || !accepts(MetadataKey::Project(repository.clone()))
            {
                continue;
            }
            model.projects.retain(|held| held.repository != repository);
            if !model.closed_projects.contains(&repository) {
                model.closed_projects.push(repository.clone());
            }
            closed_projects.push(repository);
        }
        for channel in effects.channels {
            if !accepts(MetadataKey::Channel(channel.id)) {
                continue;
            }
            match model.channels.iter_mut().find(|held| held.id == channel.id) {
                Some(held) => *held = channel,
                None => model.channels.push(channel),
            }
        }
        for id in effects.removed_channels {
            if protected_channels.contains(&id) || !accepts(MetadataKey::Channel(id)) {
                continue;
            }
            model.channels.retain(|held| held.id != id);
            removed_channels.push(id);
        }
        for order in effects.navigation_orders {
            if !accepts(MetadataKey::Navigation(order.repository.clone())) {
                continue;
            }
            model
                .navigation_orders
                .retain(|held| held.repository != order.repository);
            if !order.items.is_empty() {
                model.navigation_orders.push(order);
            }
        }
        for agent in effects.agents {
            if !accepts(MetadataKey::Agent(agent.id)) {
                continue;
            }
            match model.agents.iter_mut().find(|held| held.id == agent.id) {
                Some(held) => *held = agent,
                None => model.agents.push(agent),
            }
        }
        for id in effects.removed_agents {
            if !accepts(MetadataKey::Agent(id)) {
                continue;
            }
            model.agents.retain(|held| held.id != id);
        }
        model.sequence = model.sequence.max(sequence);
        let mut tasks = Vec::new();
        for repository in closed_projects {
            tasks.push(self.close_shared_project(&repository));
        }
        for id in removed_channels {
            if !matches!(edit, ModelEdit::RetireEmptyChannel { .. }) {
                self.forget_channel_revival(id);
            }
            tasks.push(self.retire_peer_channel(id));
            tasks.push(self.finish_channel_reopen(id));
        }
        if let ModelEdit::ReopenProject { repository, .. } = edit
            && self.workspace_model.project(repository).is_some()
        {
            self.cancel_shared_project_close(repository);
            self.project_ui.closed.remove(repository);
        }
        Some(Task::batch(tasks))
    }

    /// Installs complete views by revision, adding a scope the window lacks
    /// only when `current`; returns whether any changed.
    fn install_edge_views(&mut self, changes: ViewChanges, current: bool) -> bool {
        let views = &mut self.workspace_model.views;
        let mut changed = false;
        for view in changes.views {
            let sequence = changes
                .observed
                .get(&view.scope)
                .copied()
                .unwrap_or(changes.sequence);
            if self
                .shared_views
                .scope_absences
                .get(&view.scope)
                .is_some_and(|absence| *absence >= sequence)
            {
                continue;
            }
            self.shared_views
                .view_sequences
                .entry(view.scope.clone())
                .and_modify(|held| *held = (*held).max(sequence))
                .or_insert(sequence);
            match views.iter_mut().find(|held| held.scope == view.scope) {
                Some(held) if held.revision < view.revision => {
                    *held = view;
                    changed = true;
                }
                Some(_) => {}
                None if current => {
                    views.push(view);
                    changed = true;
                }
                None => {}
            }
        }
        for (scope, revision) in changes.removed {
            self.shared_views
                .view_sequences
                .entry(scope.clone())
                .and_modify(|held| *held = (*held).max(changes.sequence))
                .or_insert(changes.sequence);
            let newer_incarnation = views
                .iter()
                .any(|view| view.scope == scope && view.revision > revision);
            if !newer_incarnation {
                self.shared_views
                    .scope_absences
                    .entry(scope.clone())
                    .and_modify(|held| *held = (*held).max(changes.sequence))
                    .or_insert(changes.sequence);
            }
            let held = views.len();
            views.retain(|view| view.scope != scope || view.revision > revision);
            changed |= views.len() != held;
        }
        for (scope, sequence) in changes.absent {
            self.shared_views
                .scope_absences
                .entry(scope.clone())
                .and_modify(|held| *held = (*held).max(sequence))
                .or_insert(sequence);
            if sequence < self.shared_views.snapshot_sequence
                || self
                    .shared_views
                    .view_sequences
                    .get(&scope)
                    .is_some_and(|held| *held > sequence)
            {
                continue;
            }
            let count = views.len();
            views.retain(|view| view.scope != scope);
            changed |= count != views.len();
            self.shared_views.view_sequences.insert(scope, sequence);
        }
        changed
    }

    /// Shows a channel's new name from an already folded title edge.
    ///
    /// A name changes no project, channel membership, view or agent, which
    /// are everything [`Self::reconcile_workspace_model`] projects, so only
    /// the channel itself is absorbed rather than every view the window
    /// holds. Its save waits like a navigation save: the registry already
    /// holds the name, and a burst of renames is saved once. Nor does that
    /// save project views, since none of them reads a name.
    ///
    /// `None` leaves a channel this window does not show as an ordinary
    /// member, such as one it has yet to adopt or is deleting, to the full
    /// reconciliation.
    fn absorb_channel_title(&mut self, id: ChannelId) -> Option<Task<Message>> {
        if self.channel_ui.is_deleting(id) || self.channel_ui.was_deleted(id) {
            return None;
        }
        let index = self
            .durable
            .channels
            .iter()
            .position(|local| local.id == id)?;
        let shared = self.workspace_model.channel(id)?.clone();
        let mut tasks = Vec::new();
        if self.absorb_shared_channel(index, &shared) {
            self.sort_durable_channels();
            self.refresh_sidebar_channels();
            tasks.push(self.sync_chrome());
            tasks.push(self.save_snapshot_soon());
        }
        // A manual name ends a pending suggestion, and a name arriving can
        // release one this window was holding back.
        tasks.push(self.refresh_channel_title_suggestions());
        self.restore_channel_names();
        Some(Task::batch(tasks))
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
    ///
    /// Nor has this window told that registry which projects it has open: the
    /// next complete reconciliation does (see [`Self::introduce_open_projects`]).
    fn join_workspace_model(&mut self, identity: uuid::Uuid) {
        if self.durable.workspace_model_identity == Some(identity) {
            return;
        }
        self.materialize_replica_snapshot();
        let failed_seeds = std::mem::take(&mut self.shared_views.seed_failures);
        // A pending automatic composer is a window navigation intent, not
        // authority from the old registry. The new scope must initialize
        // before that exact still-selected intent can open anything.
        let deferred_default = self.shared_views.deferred_default.take();
        self.shared_views = super::shared_views::State::for_workspace(&self.durable);
        self.shared_views.deferred_default = deferred_default;
        self.clear_closed_chat_drafts();
        let durable = Arc::make_mut(&mut self.durable);
        durable.workspace_model_identity = Some(identity);
        for channel in durable.channels.iter_mut() {
            channel.shared = false;
        }
        self.persist();
        self.shared_views.seed_failures = failed_seeds;
        self.shared_views.unintroduced_projects = true;
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
        self.reconcile_workspace_model_in(None)
    }

    pub(super) fn reconcile_workspace_model_for_repository(
        &mut self,
        repository: &Path,
    ) -> Task<Message> {
        self.reconcile_workspace_model_in(Some(&[repository]))
    }

    /// Defers a background repository load's reconciliation, so a run of
    /// loads, such as every project restored at startup, reconciles and saves
    /// once rather than once per project. The deferred pass is the one each
    /// load would have run, over all of them together. It runs on
    /// [`Message::RepositoryReconciliationDue`], or before the window handles
    /// any message other than another background load. Until then a frame may
    /// draw a loaded project before the channels and panes the shared model
    /// holds for it, which the deferred pass then adds.
    ///
    /// Without a registry there is nothing to reconcile against, as in
    /// [`Self::reconcile_workspace_model`]; connecting to one reconciles
    /// everything.
    pub(super) fn defer_repository_reconciliation(&mut self, repository: PathBuf) -> Task<Message> {
        if self.workspace_model.identity == NO_REGISTRY {
            return Task::none();
        }
        let armed = !self.pending_repository_reconciliation.is_empty();
        if !self.pending_repository_reconciliation.contains(&repository) {
            self.pending_repository_reconciliation.push(repository);
        }
        if armed {
            Task::none()
        } else {
            cosmic::task::message(Message::RepositoryReconciliationDue)
        }
    }

    /// Runs the reconciliation [`Self::defer_repository_reconciliation`]
    /// deferred, if no complete one has run since.
    pub(super) fn reconcile_pending_repositories(&mut self) -> Task<Message> {
        let pending = std::mem::take(&mut self.pending_repository_reconciliation);
        if pending.is_empty() {
            return Task::none();
        }
        tracing::debug!(target: "cagents::workspace_pipeline", stage = "repository_reconciliation",
            repositories = pending.len(), "Reconciled repository loads together");
        let repositories = pending.iter().map(PathBuf::as_path).collect::<Vec<_>>();
        self.reconcile_workspace_model_in(Some(&repositories))
    }

    fn reconcile_workspace_model_in(&mut self, repositories: Option<&[&Path]>) -> Task<Message> {
        if repositories.is_none() {
            // A complete pass covers every scope a deferred one would select.
            self.pending_repository_reconciliation.clear();
        }
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
        // Written only where an order changed, so an unchanged sidebar keeps
        // its revision and the navigation list its cache.
        let navigation_order = |project: &SidebarProject| {
            model
                .navigation_orders
                .iter()
                .find(|order| order.repository == project.repository)
                .map_or(&[][..], |order| order.items.as_slice())
        };
        if self
            .sidebar_model
            .projects
            .iter()
            .any(|project| project.navigation_order != navigation_order(project))
        {
            for project in &mut self.sidebar_model.projects {
                let order = navigation_order(project);
                if project.navigation_order != order {
                    project.navigation_order = order.to_vec();
                }
            }
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
        let retired = channels_absent_from_model(&self.durable.channels, &model.channels);
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
        // Each project is opened at most once in a pass, before its channels.
        let mut opened = HashSet::new();
        if repositories.is_none() && std::mem::take(&mut self.shared_views.unintroduced_projects) {
            self.introduce_open_projects(&model, &mut opened);
        }
        for id in republished {
            self.publish_shared_channel(id, &model, &mut opened);
        }
        // Admission uses this incoming snapshot, not fences left by a previous
        // projection. A stale sibling pane is fenced by itself: it must not
        // defer an unrelated fresh actor's admission to the same scope.
        let display_index = crate::workspace_model::views::ManagedDisplayIndex::new(&model);
        self.shared_views.blocked.clear();
        self.shared_views.quarantined =
            super::shared_views::quarantined_managed_panes(&model, &model.views, &display_index);
        let mut new_managed_panes = Vec::new();
        let mut adopted_scopes = HashSet::new();
        for agent in &model.agents {
            let id = CardId::from_uuid(agent.id.as_uuid());
            // A fenced pane's saved session is not the one its agent runs, so
            // no card is created, attached or moved for it from that session.
            if self.shared_views.closed.contains(&id) || self.shared_views.quarantined.contains(&id)
            {
                continue;
            }
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
                if let Some(task) = self.adopt_execution_agent_display(agent, display) {
                    adopted_scopes.insert(scope.clone());
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
        // Normalize only sidebar ranks before the existing projection writes
        // its snapshot. The source/operation barriers and pending Inserts above
        // stay in place; no pane or canonical view metadata is changed here.
        if changed {
            self.sort_durable_channels();
        }
        // Fence canonical evidence and finish projecting authoritative pane
        // sources before reconciliation persists. Publishing earlier would mix
        // old descriptors with newly received pending-start flags.
        tasks.push(match repositories {
            Some(repositories) => {
                self.reconcile_shared_views_for_repositories(repositories, &adopted_scopes)
            }
            None => self.reconcile_shared_views(),
        });
        if changed {
            self.refresh_sidebar_channels();
            tasks.push(self.sync_chrome());
        }
        tasks.push(self.refresh_channel_title_suggestions());
        self.restore_channel_names();
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
            || self
                .shared_views
                .closed
                .contains(&CardId::from_uuid(agent.id.as_uuid()))
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
        // Canonical projection may install this proved successor after adoption
        // sees the old local descriptor. Its label is ready when that happens;
        // a replacement that matches neither source never inherits the label.
        if self.chrome_ui.agent_labels.contains_key(&agent.id) {
            self.chrome_ui
                .agent_title_sources
                .insert(card_id, session.clone());
        }
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
        let label = self.chrome_ui.agent_labels.get(&agent.id)?;
        let card = CardState {
            id: card_id,
            generation: self.durable.generation,
            // Wait for accepted metadata rather than exposing a generic label
            // that changes as soon as the conversation is first opened.
            title: display
                .pane
                .as_ref()
                .map(|pane| pane.title.clone())
                .unwrap_or_else(|| label.clone()),
            width_fraction: if let Some(pane) = &display.pane {
                pane.width_fraction
            } else if matches!(kind, CardKind::Chat(_)) {
                self.settings.default_native_pane_fraction
            } else {
                self.settings.default_column_fraction
            },
            existence: Existence::Open,
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
        // Pane names live in the window's settings, which must follow the
        // registry's, or the next start migrates this view again (#3032).
        let save_title = display.pane.as_ref().is_some_and(|pane| {
            let changed = self.chrome_ui.user_titles.get(&card_id) != pane.user_title.as_ref();
            match &pane.user_title {
                Some(title) => {
                    self.chrome_ui.user_titles.insert(card_id, title.clone());
                }
                None => {
                    self.chrome_ui.user_titles.remove(&card_id);
                }
            }
            changed
        });
        let save_title = if save_title {
            self.save_settings_task(false)
        } else {
            Task::none()
        };
        self.cards.insert(card_id, CardRuntime::Loading);
        self.register_presence(card_id, Exposure::Offscreen, Residency::Cold);
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
        let attach = Task::batch([attach, save_title]);
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
            .map(|update| self.apply_viewport(update, true, DurableChange::None))
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
        // A name cleared in another window asks for the automatic one, which
        // this window offers if it runs the channel's conversation. A model
        // older than this window's own rename looks the same, so it does not
        // forget that rename (see `channel_is_named`): the registry refuses
        // the offer once it holds the new name, whose echo then restores it.
        let cleared = retitle && !local.title.is_empty() && shared.title.is_empty();
        let channel = &mut Arc::make_mut(&mut self.durable).channels[index];
        if retitle {
            channel.title = shared.title.clone();
            channel.title_source = shared.title_source;
        }
        if rerank {
            channel.ordinal = shared.ordinal;
        }
        channel.shared = true;
        if cleared {
            self.offer_channel_name_again(shared.id);
        }
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
        // A create echo proves existence before it supplies a rank. Pending
        // local and folded remote creations must remain newer than a preceding
        // acknowledgement even when the registry was far ahead of this window.
        let has_pending = self
            .durable
            .channels
            .iter()
            .any(|channel| !channel.shared || channel.ordinal == PENDING_ORDINAL)
            || self
                .workspace_model
                .channels
                .iter()
                .any(|channel| channel.ordinal == PENDING_ORDINAL);
        if has_pending {
            let ranks: std::collections::HashMap<_, _> = self
                .workspace_model
                .channels
                .iter()
                .map(|channel| (channel.id, channel.ordinal))
                .collect();
            let pending = |channel: &ProjectChannel| {
                !channel.shared
                    || channel.ordinal == PENDING_ORDINAL
                    || ranks.get(&channel.id) == Some(&PENDING_ORDINAL)
            };
            let mut provisional: Vec<_> = self
                .durable
                .channels
                .iter()
                .enumerate()
                .filter(|(_, channel)| pending(channel))
                .map(|(index, channel)| (index, channel.ordinal))
                .collect();
            if !provisional.is_empty() {
                provisional.sort_by_key(|(_, ordinal)| *ordinal);
                let highest = self
                    .workspace_model
                    .channels
                    .iter()
                    .map(|channel| channel.ordinal)
                    .chain(
                        self.durable
                            .channels
                            .iter()
                            .filter(|channel| !pending(channel))
                            .map(|channel| channel.ordinal),
                    )
                    .filter(|ordinal| *ordinal != PENDING_ORDINAL)
                    .max()
                    .unwrap_or(0);
                let mut next = Some(highest);
                let updates: Vec<_> = provisional
                    .into_iter()
                    .filter_map(|(index, ordinal)| {
                        next = next
                            .and_then(|rank| rank.checked_add(1))
                            .filter(|rank| *rank != PENDING_ORDINAL);
                        let rank = next.unwrap_or(PENDING_ORDINAL);
                        (rank != ordinal).then_some((index, rank))
                    })
                    .collect();
                if !updates.is_empty() {
                    let durable = Arc::make_mut(&mut self.durable);
                    for (index, rank) in updates {
                        durable.channels[index].ordinal = rank;
                    }
                }
            }
        }
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
            .filter(|ordinal| *ordinal != PENDING_ORDINAL)
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
        // A new channel's title is empty; a revived one keeps the name it had.
        let (title, title_source) = self
            .durable
            .channels
            .iter()
            .find(|channel| channel.id == id)
            .map(|channel| (channel.title.clone(), channel.title_source))
            .unwrap_or_default();
        self.publish_model_edit(ModelEdit::CreateChannel {
            id,
            repository,
            title,
            title_source,
        });
    }

    /// Publishes a channel this window holds and the registry has never heard
    /// of, which is how a window that predates the registry - or that was open
    /// while a replacement one was created - introduces what it already has.
    ///
    /// `model` is the registry's model, which a reconciliation holds apart
    /// from the window while it runs; `opened` holds the repositories this
    /// pass has already opened.
    fn publish_shared_channel(
        &mut self,
        id: ChannelId,
        model: &WorkspaceModel,
        opened: &mut HashSet<PathBuf>,
    ) {
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
        if !holds_project(model, &repository, &project.name, project.kind)
            && opened.insert(repository.clone())
        {
            self.publish_model_edit(ModelEdit::OpenProject {
                repository: repository.clone(),
                name: project.name.clone(),
                kind: project.kind,
            });
        }
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
        self.project_ui.opened.insert(repository.clone());
        if self.project_ui.is_reopening(&repository) {
            self.publish_model_edit(ModelEdit::ReopenProject {
                repository,
                name,
                kind,
            });
        } else if !holds_project(&self.workspace_model, &repository, &name, kind) {
            self.publish_model_edit(ModelEdit::OpenProject {
                repository,
                name,
                kind,
            });
        }
    }

    /// Tells a registry this window has just joined about the projects it has
    /// open and the registry lacks, adding each to `opened`.
    ///
    /// A repository load skips `OpenProject` for a project the registry it
    /// last heard from holds (see [`holds_project`]). When that registry is
    /// replaced, its successor never received those opens, so they are sent
    /// here for every project a load has opened. A project still loading opens
    /// itself when its load lands, against the successor's model. A project
    /// the successor tombstoned closes instead, or, if the user is reopening
    /// it, sends its own `ReopenProject`, which a plain open would only be
    /// refused ahead of. One whose checkout a later read could not reach is
    /// not opened anywhere it is missing.
    fn introduce_open_projects(&mut self, model: &WorkspaceModel, opened: &mut HashSet<PathBuf>) {
        let unknown = self
            .sidebar_model
            .projects
            .iter()
            .filter(|project| {
                self.project_ui.opened.contains(&project.repository)
                    && project.unreachable.is_none()
                    && !self.project_ui.closed.contains(&project.repository)
                    && !model.closed_projects.contains(&project.repository)
                    && !holds_project(model, &project.repository, &project.name, project.kind)
                    && opened.insert(project.repository.clone())
            })
            .map(|project| ModelEdit::OpenProject {
                repository: project.repository.clone(),
                name: project.name.clone(),
                kind: project.kind,
            })
            .collect::<Vec<_>>();
        for edit in unknown {
            self.publish_model_edit(edit);
        }
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
        let request = ModelRequest::from(edit);
        let seed = match &request.edit {
            ModelEdit::SeedView { scope, .. } => {
                Some((scope.clone(), request.seed_attempt.unwrap()))
            }
            _ => None,
        };
        if let Some((scope, attempt)) = &seed {
            self.shared_views
                .seed_attempts
                .insert(scope.clone(), *attempt);
        }
        if let Err(error) = self.workspace_model_tx.try_send(request) {
            if let Some((scope, attempt)) = seed {
                self.refuse_view_seed(
                    scope,
                    attempt,
                    "The workspace connection is unavailable".into(),
                );
            }
            tracing::debug!(%error, "a shared workspace model edit was not queued");
            if navigation {
                self.status = Some(TransientNotice::warning(
                    "Could not save the sidebar order; try again once the selected device is connected",
                ));
            }
        }
    }
}

/// Inspect model edits in fixtures without coupling their assertions to local
/// publisher correlation metadata. The real request still enters the same queue.
#[cfg(test)]
pub(in crate::app) struct TestModelRequests(async_channel::Receiver<ModelRequest>);

#[cfg(test)]
impl TestModelRequests {
    pub(in crate::app) fn try_recv(&self) -> Result<ModelEdit, async_channel::TryRecvError> {
        self.0.try_recv().map(|request| request.edit)
    }
}

#[cfg(test)]
pub(in crate::app) fn test_model_channel()
-> (async_channel::Sender<ModelRequest>, TestModelRequests) {
    let (sender, requests) = async_channel::unbounded();
    (sender, TestModelRequests(requests))
}

#[cfg(test)]
mod tests;
