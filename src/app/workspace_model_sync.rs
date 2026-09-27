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
        // A create's rank is assigned by the registry, so it is not in the edit.
        // Reading the model afterwards is how an adopting window learns where
        // the new channel sorts, and it is one request on a connection that is
        // otherwise idle until the next edge.
        let ranked = matches!(
            edit,
            ModelEdit::CreateChannel { .. } | ModelEdit::AgentsChanged
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
    let created = match &edit {
        ModelEdit::CreateChannel { id, .. } => Some(*id),
        _ => None,
    };
    let reopened = match &edit {
        ModelEdit::ReopenProject { repository, .. } => Some(repository.clone()),
        _ => None,
    };
    let model = client.edit_workspace_model(edit)?;
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
    if let Some(repository) = reopened {
        let _ = updates.send_blocking(ModelUpdate::Reopened { repository, model });
    }
    Ok(())
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
    pub(super) fn apply_workspace_model_update(&mut self, update: ModelUpdate) -> Task<Message> {
        match update {
            ModelUpdate::Connected(model) => {
                if model.identity == self.workspace_model.identity
                    && model.sequence < self.workspace_model.sequence
                {
                    return Task::none();
                }
                self.join_workspace_model(model.identity);
                self.workspace_model = model;
                let closed = self.apply_closed_project_tombstones();
                Task::batch([closed, self.reconcile_workspace_model()])
            }
            ModelUpdate::Read(model) => {
                if model.identity == self.workspace_model.identity
                    && model.sequence < self.workspace_model.sequence
                {
                    return Task::none();
                }
                self.workspace_model = model;
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
                self.workspace_model = model;
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
                    self.delete_channel(id)
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
                tasks.push(self.delete_channel(id));
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
        for agent in &model.agents {
            // Scheduled agents stay headless until an explicit history or
            // notification action opens their exact live run.
            if matches!(
                agent.origin,
                crate::agent_execution::Origin::Automation { .. }
            ) {
                continue;
            }
            if let Some(task) = self.adopt_execution_agent(agent) {
                changed = true;
                tasks.push(task);
            }
        }
        self.workspace_model = model;
        if changed {
            self.sort_durable_channels();
            self.refresh_sidebar_channels();
            self.persist();
            tasks.push(self.sync_chrome());
        }
        Task::batch(tasks)
    }

    /// Adoption records the daemon's exact descriptor without launching a
    /// replacement, switching channels, or taking focus from current work.
    pub(super) fn adopt_execution_agent(
        &mut self,
        agent: &crate::agent_execution::Agent,
    ) -> Option<Task<Message>> {
        if agent.closed
            || self.channel_ui.is_deleting(agent.channel)
            || self.channel_ui.was_deleted(agent.channel)
        {
            return None;
        }
        let selected;
        let session = if agent.spec.surface == crate::agent_execution::Surface::Terminal {
            selected = crate::agent_execution::HostedSession::Terminal(agent.console.clone()?);
            &selected
        } else {
            agent.session.as_ref()?
        };
        let channel_index = self
            .durable
            .channels
            .iter()
            .position(|channel| channel.id == agent.channel)?;
        let card_id = CardId::from_uuid(agent.id.as_uuid());
        // Includes tombstones: an explicit local close is never undone by an
        // old model read, a reconnect, or another window observing this agent.
        if self.durable.cards.iter().any(|card| card.id == card_id) {
            return None;
        }
        let kind = match session {
            crate::agent_execution::HostedSession::Chat(session) => CardKind::Chat(session.clone()),
            crate::agent_execution::HostedSession::Terminal(session) => {
                CardKind::Terminal(session.clone())
            }
        };
        let card = CardState {
            id: card_id,
            generation: self.durable.generation,
            title: format!("{} agent", agent.spec.provider.label()),
            width_fraction: self.settings.default_column_fraction,
            existence: Existence::Open,
            exposure: Exposure::Offscreen,
            residency: Residency::Cold,
            kind,
        };
        let durable = Arc::make_mut(&mut self.durable);
        durable.cards.push(card.clone());
        durable
            .card_scopes
            .insert(card_id, agent.spec.target.directory.clone());
        let channel = &mut durable.channels[channel_index];
        channel.cards.push(card_id);
        channel.focused.get_or_insert(card_id);
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
        let attach = match session {
            crate::agent_execution::HostedSession::Chat(session) => {
                self.open_chat(card_id, session.clone(), crate::chat::Intent::Restore)
            }
            crate::agent_execution::HostedSession::Terminal(_) => Task::none(),
        };
        if self.durable.active_channel != Some(agent.channel) {
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
        let retitle = local.title != shared.title;
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
        if self.project_for_repository(&shared.repository).is_some() {
            return Adoption::Retained;
        }
        self.open_shared_repository(&shared.repository, Some(shared.kind))
    }

    /// Reads a repository the shared model names and this window has not opened.
    fn open_shared_repository(&mut self, repository: &Path, kind: Option<ProjectKind>) -> Adoption {
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
    fn project_for_repository(&self, repository: &Path) -> Option<ProjectId> {
        self.sidebar_model
            .projects
            .iter()
            .find(|project| {
                project.repository == repository
                    || canonical_repository(&project.repository)
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
        if let Err(error) = self.workspace_model_tx.try_send(edit) {
            tracing::debug!(%error, "a shared workspace model edit was not queued");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            id: crate::model::AgentId::new(),
            origin: Origin::User,
            spec: Spec {
                provider: Provider::Claude,
                account: None,
                target: Target {
                    repository,
                    directory: PathBuf::from("/repo/worktree"),
                    channel: Some(target),
                },
                surface: Surface::Chat,
                model: None,
                permission_mode: None,
            },
            channel: target,
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
            id,
            repository: repository.to_path_buf(),
            title: title.to_owned(),
            ordinal,
        }
    }

    fn local(id: ChannelId, project: ProjectId, title: &str, ordinal: u32) -> ProjectChannel {
        ProjectChannel {
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
            identity: uuid::Uuid::new_v4(),
            sequence: channels.len() as u64,
            projects: Vec::new(),
            channels,
            closed_projects: Vec::new(),
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
    /// retired by the same path a local deletion uses.
    #[test]
    fn a_deleted_channel_retires_its_cards_through_the_local_path() {
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
        assert!(app.channel_ui.is_deleting(id));
        assert!(!drain(&published).iter().any(|edit| matches!(edit, ModelEdit::CreateChannel { id: revived, .. } if *revived == id)), "an in-flight deletion must not resurrect an unacknowledged channel");
        let _ = app.handle_channel_ui(super::super::channel_ui::Action::SessionsClosed(id, Ok(())));

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
        assert_eq!(
            drain(&published),
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

        let _ = app.handle_channel_ui(super::super::channel_ui::Action::SessionsClosed(id, Ok(())));

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
