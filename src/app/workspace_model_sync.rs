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
    ModelChannel, ModelEdit, ModelProject, PENDING_ORDINAL, WorkspaceModel, canonical_repository,
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
    /// why it is distinct from [`ModelUpdate::Read`]: it is what makes the
    /// window offer its own state again to a registry that may have been
    /// replaced, rather than trusting a flag from a previous connection.
    Connected(WorkspaceModel),
    /// A fresh read, taken because an edge does not carry the ranks it changed.
    Read(WorkspaceModel),
    /// The registry's answer to this window's seed.
    ///
    /// From here the registry is authoritative: it has heard everything this
    /// window had, so a channel missing from it was deleted somewhere rather
    /// than never known.
    Seeded(WorkspaceModel),
    Edited {
        sequence: u64,
        edit: ModelEdit,
    },
    /// The rank the registry gave a channel this window created.
    Ranked {
        channel: ChannelId,
        ordinal: u32,
    },
}

/// What the window asks the publisher to send.
#[derive(Debug, Clone)]
pub enum ModelPublish {
    Edit(ModelEdit),
    Seed {
        projects: Vec<ModelProject>,
        channels: Vec<ModelChannel>,
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
        let ranked = matches!(edit, ModelEdit::CreateChannel { .. });
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
    requests: async_channel::Receiver<ModelPublish>,
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
    requests: &async_channel::Receiver<ModelPublish>,
    pending: &mut Option<ModelPublish>,
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
        match send(&mut client, request, updates) {
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
                }
                return Err(error.to_string());
            }
        }
    }
}

fn send(
    client: &mut ControlClient,
    request: ModelPublish,
    updates: &async_channel::Sender<ModelUpdate>,
) -> Result<(), ControlError> {
    match request {
        ModelPublish::Edit(edit) => {
            let created = match &edit {
                ModelEdit::CreateChannel { id, .. } => Some(*id),
                _ => None,
            };
            let model = client.edit_workspace_model(edit)?;
            // The originating client is deliberately not sent its own echo, so
            // this response is how the window that minted the channel learns the
            // rank the registry gave it.
            if let Some(id) = created
                && let Some(channel) = model.channel(id)
            {
                let _ = updates.send_blocking(ModelUpdate::Ranked {
                    channel: id,
                    ordinal: channel.ordinal,
                });
            }
        }
        ModelPublish::Seed { projects, channels } => {
            let model = client.seed_workspace_model(projects, channels)?;
            let _ = updates.send_blocking(ModelUpdate::Seeded(model));
        }
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
                // A new connection may be to a new registry - a replaced daemon,
                // or a data directory that was wiped - so authority is earned
                // again rather than carried over from the previous one.
                self.workspace_model_seeded = false;
                self.workspace_model = model;
                let reconciled = self.reconcile_workspace_model();
                self.offer_workspace_model_seed();
                reconciled
            }
            ModelUpdate::Read(model) => {
                self.workspace_model = model;
                self.reconcile_workspace_model()
            }
            ModelUpdate::Seeded(model) => {
                self.workspace_model_seeded = true;
                self.workspace_model = model;
                self.reconcile_workspace_model()
            }
            ModelUpdate::Edited { sequence, edit } => {
                // An explicit delete is an instruction rather than an absence,
                // so folding it in retires the channel even before this window
                // has seeded - which is exactly the case absence may not be
                // trusted in.
                let deleted = match &edit {
                    ModelEdit::DeleteChannel { id } => Some(*id),
                    _ => None,
                };
                self.workspace_model.fold(sequence, &edit);
                let retired = deleted.map_or_else(Task::none, |id| self.delete_channel(id));
                Task::batch([retired, self.reconcile_workspace_model()])
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
                if self.durable.channels[index].ordinal == ordinal {
                    return Task::none();
                }
                Arc::make_mut(&mut self.durable).channels[index].ordinal = ordinal;
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

    /// Offers this window's sidebar state to a registry no window has seeded.
    ///
    /// The registry applies it only while it has never been seeded, so sending
    /// it unconditionally is safe and saves the window a flag it would have to
    /// read across the wire. The projects and channels go in this window's own
    /// durable order, oldest first, because that is the order the registry ranks
    /// them in.
    fn offer_workspace_model_seed(&mut self) {
        let projects = self
            .sidebar_model
            .projects
            .iter()
            .map(|project| ModelProject {
                repository: project.repository.clone(),
                name: project.name.clone(),
                ordinal: 0,
            })
            .collect::<Vec<_>>();
        let channels = self
            .durable
            .channels
            .iter()
            .filter_map(|channel| {
                let project = self.sidebar_model.project(channel.project_id)?;
                Some(ModelChannel {
                    id: channel.id,
                    repository: project.repository.clone(),
                    title: channel.title.clone(),
                    ordinal: 0,
                })
            })
            .collect::<Vec<_>>();
        self.queue_workspace_model_publish(ModelPublish::Seed { projects, channels });
    }

    /// Mirrors the cached shared model into this window's own store.
    ///
    /// Additive until the registry has been seeded. An empty registry means no
    /// window has ever offered its state, not that everything was deleted, so
    /// deleting on absence before that would let a fresh registry - a replaced
    /// daemon, a wiped data directory - retire every channel the user has.
    pub(super) fn reconcile_workspace_model(&mut self) -> Task<Message> {
        // Taken rather than cloned: reconciling needs `&mut self` for the
        // window's own store, and the model is put back untouched.
        let model = std::mem::take(&mut self.workspace_model);
        let mut tasks = Vec::new();
        let mut changed = false;
        // Projects first, so a channel's owner exists by the time the channel is
        // considered.
        for shared in &model.projects {
            match self.adopt_shared_project(shared) {
                Adoption::Applied => changed = true,
                Adoption::Opening(task) => tasks.push(task),
                Adoption::Retained => {}
            }
        }
        for shared in &model.channels {
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
                None => match self.adopt_shared_channel(shared) {
                    Adoption::Applied => changed = true,
                    Adoption::Opening(task) => tasks.push(task),
                    Adoption::Retained => {}
                },
            }
        }
        if self.workspace_model_seeded {
            let retired = self
                .durable
                .channels
                .iter()
                .filter(|local| !model.channels.iter().any(|shared| shared.id == local.id))
                .map(|local| local.id)
                .collect::<Vec<_>>();
            for id in retired {
                // The window's own deletion path, so a channel that disappeared
                // elsewhere retires its cards exactly as a local delete retires
                // them.
                tasks.push(self.delete_channel(id));
                changed = true;
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
        if !retitle && !rerank {
            return false;
        }
        let channel = &mut Arc::make_mut(&mut self.durable).channels[index];
        if retitle {
            channel.title = shared.title.clone();
        }
        if rerank {
            channel.ordinal = shared.ordinal;
        }
        true
    }

    /// Shows a channel this window does not have.
    fn adopt_shared_channel(&mut self, shared: &ModelChannel) -> Adoption {
        let Some(project_id) = self.project_for_repository(&shared.repository) else {
            return self.open_shared_repository(&shared.repository);
        };
        Arc::make_mut(&mut self.durable)
            .channels
            .push(ProjectChannel {
                id: shared.id,
                project_id,
                title: shared.title.clone(),
                ordinal: shared.ordinal,
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
        if self.project_for_repository(&shared.repository).is_some() {
            return Adoption::Retained;
        }
        self.open_shared_repository(&shared.repository)
    }

    /// Reads a repository the shared model names and this window has not opened.
    fn open_shared_repository(&mut self, repository: &Path) -> Adoption {
        // Once per path. `repository_load_task` mints a fresh `ProjectId` when
        // given none, so asking twice for one repository would leave the window
        // with two projects on one checkout.
        if !self.workspace_model_opens.insert(repository.to_path_buf()) {
            return Adoption::Retained;
        }
        if !repository.join(".git").exists() {
            // Not a checkout this window can read - it may live on a filesystem
            // that is not mounted here. The row stays in the model and renders
            // nothing, rather than becoming a project that cannot be loaded.
            return Adoption::Retained;
        }
        Adoption::Opening(self.repository_load_task(repository.to_path_buf(), None, true))
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

    /// Publishes this window's creation of a channel.
    pub(super) fn publish_channel_creation(&mut self, id: ChannelId, project_id: ProjectId) {
        let Some(project) = self.sidebar_model.project(project_id) else {
            return;
        };
        let repository = project.repository.clone();
        let name = project.name.clone();
        // The project first: a channel in a repository no project owns is
        // reachable from no sidebar. Both edits go through one queue, so the
        // registry sees them in this order.
        self.queue_workspace_model_publish(ModelPublish::Edit(ModelEdit::OpenProject {
            repository: repository.clone(),
            name,
        }));
        self.queue_workspace_model_publish(ModelPublish::Edit(ModelEdit::CreateChannel {
            id,
            repository,
            title: String::new(),
        }));
    }

    /// Publishes a project this window opened.
    pub(super) fn publish_project_opened(&mut self, project_id: ProjectId) {
        let Some(project) = self.sidebar_model.project(project_id) else {
            return;
        };
        let edit = ModelEdit::OpenProject {
            repository: project.repository.clone(),
            name: project.name.clone(),
        };
        self.queue_workspace_model_publish(ModelPublish::Edit(edit));
    }

    /// Queues one shared-model edit without waiting for it.
    ///
    /// A redraw must never wait on the daemon, so this only queues: the
    /// publisher thread owns the connection and the retry, and the window learns
    /// the outcome as an update like any other.
    pub(super) fn publish_model_edit(&mut self, edit: ModelEdit) {
        self.queue_workspace_model_publish(ModelPublish::Edit(edit));
    }

    fn queue_workspace_model_publish(&mut self, request: ModelPublish) {
        if let Err(error) = self.workspace_model_tx.try_send(request) {
            tracing::debug!(%error, "a shared workspace model edit was not queued");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app_with_project() -> (Cagents, ProjectId, PathBuf) {
        let mut app = super::super::tests::context_inspector_test_app(true);
        let project = ProjectId::new();
        let repository = PathBuf::from("/repo");
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

    /// A channel another window created arrives as a scope, not as a
    /// conversation: N windows adopting one channel must not start N chats in
    /// it, and the cards inside a channel are each window's own business.
    #[test]
    fn an_adopted_channel_arrives_empty_and_starts_no_conversation() {
        let (mut app, project, repository) = app_with_project();
        let cards = app.durable.cards.len();
        let id = ChannelId::new();
        app.workspace_model
            .channels
            .push(shared(id, &repository, "created elsewhere", 7));

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
    }

    /// The registry's rank decides the order, so an adopted channel created
    /// after a local one renders above it - in every window, whichever one made
    /// which.
    #[test]
    fn a_newer_adopted_channel_renders_above_an_older_local_one() {
        let (mut app, project, repository) = app_with_project();
        let local = ChannelId::new();
        Arc::make_mut(&mut app.durable)
            .channels
            .push(ProjectChannel {
                id: local,
                project_id: project,
                title: "mine".into(),
                ordinal: 3,
                cards: Vec::new(),
                focused: None,
            });
        let adopted = ChannelId::new();
        app.workspace_model
            .channels
            .push(shared(local, &repository, "mine", 3));
        app.workspace_model
            .channels
            .push(shared(adopted, &repository, "theirs", 9));

        let _ = app.reconcile_workspace_model();
        app.refresh_sidebar_channels();

        let rows = &app.sidebar_model.projects[0].channels;
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].id, adopted, "the newer channel is on top");
        assert_eq!(rows[1].id, local);
    }

    /// A channel deleted anywhere disappears everywhere, and its cards are
    /// retired by the same path a local deletion uses.
    #[test]
    fn a_deleted_channel_retires_its_cards_through_the_local_path() {
        let (mut app, project, repository) = app_with_project();
        let id = ChannelId::new();
        let conversation = app.durable.cards[0].id;
        Arc::make_mut(&mut app.durable)
            .channels
            .push(ProjectChannel {
                id,
                project_id: project,
                title: "doomed".into(),
                ordinal: 4,
                cards: vec![conversation],
                focused: Some(conversation),
            });
        app.workspace_model
            .channels
            .push(shared(id, &repository, "doomed", 4));
        app.workspace_model_seeded = true;

        let _ = app.apply_workspace_model_update(ModelUpdate::Edited {
            sequence: 5,
            edit: ModelEdit::DeleteChannel { id },
        });

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

    /// An empty registry means no window has seeded it, not that every channel
    /// was deleted. Retiring a user's channels because a fresh daemon has never
    /// heard of them would destroy exactly what the shared model exists to keep.
    #[test]
    fn an_unseeded_registry_never_retires_a_local_channel() {
        let (mut app, project, _repository) = app_with_project();
        let id = ChannelId::new();
        Arc::make_mut(&mut app.durable)
            .channels
            .push(ProjectChannel {
                id,
                project_id: project,
                title: "mine".into(),
                ordinal: 0,
                cards: Vec::new(),
                focused: None,
            });

        let _ = app.apply_workspace_model_update(ModelUpdate::Connected(WorkspaceModel::default()));

        assert!(
            app.durable.channels.iter().any(|channel| channel.id == id),
            "an unseeded registry is not an authority to delete by"
        );
        assert!(!app.workspace_model_seeded);
    }

    /// The rank the registry assigns replaces the provisional one a local
    /// creation started with, so the window that created a channel and the
    /// window that adopted it agree on where it sorts.
    #[test]
    fn the_registry_rank_replaces_the_provisional_one() {
        let (mut app, project, _repository) = app_with_project();
        let id = ChannelId::new();
        Arc::make_mut(&mut app.durable)
            .channels
            .push(ProjectChannel {
                id,
                project_id: project,
                title: String::new(),
                ordinal: PENDING_ORDINAL,
                cards: Vec::new(),
                focused: None,
            });

        let _ = app.apply_workspace_model_update(ModelUpdate::Ranked {
            channel: id,
            ordinal: 11,
        });

        assert_eq!(
            app.durable
                .channels
                .iter()
                .find(|channel| channel.id == id)
                .unwrap()
                .ordinal,
            11
        );
    }
}
