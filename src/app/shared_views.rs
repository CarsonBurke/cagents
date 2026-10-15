//! A client projection of daemon-owned scope views. Pending local intentions
//! are layered over snapshots until acknowledged; snapshots never erase them.
use super::*;
use crate::workspace_model::{
    ModelEdit,
    views::{self, Change, Pane, Scope, View},
};

/// One immutable projection for a publication pass. Scope discovery includes
/// historical directories, but their cards never become open shared panes.
struct LocalViews {
    scopes: Vec<Scope>,
    cards: HashMap<Scope, Vec<CardId>>,
    panes: HashMap<Scope, Vec<Pane>>,
}

#[derive(Default)]
pub(super) struct State {
    baseline: HashMap<Scope, Vec<Pane>>,
    initialized: HashSet<Scope>,
    seeding: HashSet<Scope>,
    pending: Vec<(uuid::Uuid, Scope, Change)>,
    /// Explicit moves wait for host acceptance before changing local membership.
    requested_transfers: HashMap<uuid::Uuid, CardId>,
    pub attached: HashSet<CardId>,
    retained: HashSet<CardId>,
    applying: bool,
    pub creating_default: bool,
    pub deferred_default: Option<PathBuf>,
    pub defaults: HashMap<CardId, TerminalSession>,
    default_requests: HashSet<CardId>,
    pub blocked: HashSet<Scope>,
    pub adoptions: Vec<crate::persistence::TerminalPaneAdoption>,
    pub restorations: Vec<crate::persistence::TerminalPaneRestoration>,
    restoration_requests: HashMap<CardId, TerminalId>,
}

impl State {
    pub(super) fn is_requested_transfer(&self, operation: uuid::Uuid) -> bool {
        self.requested_transfers.contains_key(&operation)
    }

    /// Retire exactly the refused intent; unrelated in-flight edits still
    /// project over the authoritative view until their own acknowledgments.
    pub(super) fn refused(&mut self, operation: uuid::Uuid) {
        self.requested_transfers.remove(&operation);
        self.pending.retain(|(id, _, _)| *id != operation);
    }

    pub(super) fn prune_restoration_requests(&mut self, workspace: &WorkspaceState) {
        if self.restoration_requests.is_empty() {
            return;
        }
        let open = workspace
            .cards
            .iter()
            .filter_map(|card| {
                if card.existence.is_open() {
                    if let CardKind::Terminal(session) = &card.kind {
                        return Some((card.id, session.id));
                    }
                }
                None
            })
            .collect::<HashMap<_, _>>();
        self.restoration_requests
            .retain(|card, terminal| open.get(card) == Some(terminal));
    }

    pub(super) fn for_workspace(workspace: &WorkspaceState) -> Self {
        let mut state = Self::default();
        state.retained = workspace.local_only_cards.clone();
        state.attached = workspace
            .channels
            .iter()
            .filter(|scope| scope.shared)
            .flat_map(|scope| scope.cards.iter().copied())
            .chain(
                workspace
                    .cards
                    .iter()
                    .filter(|card| {
                        workspace.workspace_model_identity.is_some()
                            && card.existence.is_open()
                            && matches!(card.kind, CardKind::Terminal(_) | CardKind::Chat(_))
                    })
                    .map(|card| card.id),
            )
            .collect();
        state
    }
}

impl Cagents {
    pub(super) fn retain_local_pane(&mut self, id: CardId) {
        self.shared_views.retained.insert(id);
        Arc::make_mut(&mut self.durable).local_only_cards.insert(id);
    }

    pub(super) fn reserve_terminal_creation(&mut self, id: CardId) {
        // Reserve in the selected, registered view. A directory under a
        // folder project may belong to a shared child not yet loaded locally;
        // that is insufficient authority to pin it to the parent view.
        let scope = self.durable.active_channel.map(Scope::Channel).or_else(|| {
            self.active_workspace
                .as_deref()
                .and_then(|path| self.worktree_view_scope(path))
        });
        if scope.is_some() {
            // Preserve directory-derived ownership until the daemon accepts
            // the pane. A shared child may still be loading in the sidebar.
            Arc::make_mut(&mut self.durable)
                .pending_terminal_creations
                .insert(id);
        }
    }

    pub(super) fn worktree_view_scope(&self, path: &Path) -> Option<Scope> {
        let project = self.sidebar_model.projects.iter().find(|project| {
            project
                .worktrees
                .iter()
                .any(|worktree| worktree.path == path)
                || project.repository == path
                || (project
                    .repository
                    .file_name()
                    .is_some_and(|name| name == ".git")
                    && project.repository.parent() == Some(path))
        })?;
        Some(Scope::Worktree {
            repository: project.repository.clone(),
            path: path.to_path_buf(),
        })
    }

    fn local_view_scopes(&self) -> Vec<Scope> {
        let mut scopes = self
            .durable
            .channels
            .iter()
            .map(|channel| Scope::Channel(channel.id))
            .collect::<Vec<_>>();
        for scope in self.workspace_model.views.iter().map(|view| &view.scope) {
            if let Scope::Worktree { repository, .. } = scope {
                if self
                    .sidebar_model
                    .projects
                    .iter()
                    .any(|project| &project.repository == repository)
                {
                    scopes.push(scope.clone());
                }
            }
        }
        let paths = self
            .known_worktree_roots
            .iter()
            .map(PathBuf::as_path)
            .chain(self.active_workspace.as_deref())
            .chain(
                self.durable
                    .cards
                    .iter()
                    .filter_map(|card| self.card_worktree(card)),
            );
        for path in paths {
            if let Some(scope) = self.worktree_view_scope(path) {
                scopes.push(scope);
            }
        }
        let mut seen = HashSet::new();
        scopes.retain(|scope| seen.insert(scope.clone()));
        scopes
    }

    fn local_view_cards(&self, scope: &Scope) -> Vec<CardId> {
        match scope {
            Scope::Channel(id) => self
                .durable
                .channels
                .iter()
                .find(|channel| channel.id == *id)
                .map(|channel| channel.cards.clone())
                .unwrap_or_default(),
            Scope::Worktree { path, .. } => {
                let roots = self.indexed_worktree_roots();
                let channel_cards = self
                    .durable
                    .channels
                    .iter()
                    .flat_map(|channel| channel.cards.iter().copied())
                    .collect::<HashSet<_>>();
                self.durable
                    .cards
                    .iter()
                    .filter(|card| {
                        card.existence.is_open()
                            && !card.kind.is_utility()
                            && !channel_cards.contains(&card.id)
                            && self.unchanneled_card_worktree_indexed(card, &roots)
                                == Some(path.as_path())
                    })
                    .map(|card| card.id)
                    .collect()
            }
        }
    }

    fn local_view_panes(&self, scope: &Scope) -> Vec<Pane> {
        self.local_view_cards(scope)
            .into_iter()
            .filter_map(|id| {
                if self.shared_views.retained.contains(&id)
                    || self.chat_is_unused_default(id)
                    || self.shared_views.defaults.contains_key(&id)
                {
                    return None;
                }
                let mut pane = Pane::from_card(
                    self.card_state(id)?,
                    self.durable.card_scopes.get(&id).cloned(),
                )?;
                pane.user_title = self.chrome_ui.user_titles.get(&id).cloned();
                pane.pending_terminal_start = matches!(pane.kind, CardKind::Terminal(_))
                    && (self.durable.pending_terminal_creations.contains(&id)
                        || self
                            .workspace_model
                            .views
                            .iter()
                            .flat_map(|view| &view.panes)
                            .any(|shared| shared.id == id && shared.pending_terminal_start));
                if let Scope::Worktree { path, .. } = scope {
                    pane.scope = Some(path.clone());
                }
                Some(pane)
            })
            .collect()
    }

    fn local_views(&self) -> LocalViews {
        let roots = self.indexed_worktree_roots();
        let cards = self
            .durable
            .cards
            .iter()
            .map(|card| (card.id, card))
            .collect::<HashMap<_, _>>();
        let channel_cards = self
            .durable
            .channels
            .iter()
            .flat_map(|channel| channel.cards.iter().copied())
            .collect::<HashSet<_>>();
        // Preserve the first matching project's authority, just as
        // worktree_view_scope does, without searching every worktree per path.
        let mut repositories = HashMap::new();
        for project in &self.sidebar_model.projects {
            for path in project
                .worktrees
                .iter()
                .map(|worktree| worktree.path.as_path())
                .chain(std::iter::once(project.repository.as_path()))
                .chain(
                    project
                        .repository
                        .file_name()
                        .is_some_and(|name| name == ".git")
                        .then(|| project.repository.parent())
                        .flatten(),
                )
            {
                repositories.entry(path).or_insert(&project.repository);
            }
        }
        let worktree_cards = self
            .durable
            .cards
            .iter()
            .filter(|card| !channel_cards.contains(&card.id))
            .filter_map(|card| {
                self.unchanneled_card_worktree_indexed(card, &roots)
                    .map(|path| (card, path))
            })
            .collect::<Vec<_>>();
        let mut scopes = self
            .durable
            .channels
            .iter()
            .map(|channel| Scope::Channel(channel.id))
            .collect::<Vec<_>>();
        let project_repositories = self
            .sidebar_model
            .projects
            .iter()
            .map(|project| &project.repository)
            .collect::<HashSet<_>>();
        scopes.extend(
            self.workspace_model
                .views
                .iter()
                .filter_map(|view| match &view.scope {
                    Scope::Worktree { repository, .. }
                        if project_repositories.contains(repository) =>
                    {
                        Some(view.scope.clone())
                    }
                    _ => None,
                }),
        );
        for path in self
            .known_worktree_roots
            .iter()
            .map(PathBuf::as_path)
            .chain(self.active_workspace.as_deref())
            .chain(worktree_cards.iter().map(|(_, path)| *path))
        {
            if let Some(repository) = repositories.get(path) {
                scopes.push(Scope::Worktree {
                    repository: (*repository).clone(),
                    path: path.to_owned(),
                });
            }
        }
        let mut seen = HashSet::new();
        scopes.retain(|scope| seen.insert(scope.clone()));
        let mut by_path = HashMap::<&Path, Vec<CardId>>::new();
        for (card, path) in worktree_cards {
            if card.existence.is_open() && !card.kind.is_utility() {
                by_path.entry(path).or_default().push(card.id);
            }
        }
        let mut members = self
            .durable
            .channels
            .iter()
            .map(|channel| (Scope::Channel(channel.id), channel.cards.clone()))
            .collect::<HashMap<_, _>>();
        let pending = self
            .workspace_model
            .views
            .iter()
            .flat_map(|view| &view.panes)
            .filter(|pane| pane.pending_terminal_start)
            .map(|pane| pane.id)
            .collect::<HashSet<_>>();
        let mut panes = HashMap::new();
        for scope in &scopes {
            if let Scope::Worktree { path, .. } = scope {
                members.insert(
                    scope.clone(),
                    by_path.get(path.as_path()).cloned().unwrap_or_default(),
                );
            }
            let projected = members[scope]
                .iter()
                .filter_map(|id| {
                    if self.shared_views.retained.contains(id)
                        || self.chat_is_unused_default(*id)
                        || self.shared_views.defaults.contains_key(id)
                    {
                        return None;
                    }
                    let mut pane =
                        Pane::from_card(cards.get(id)?, self.durable.card_scopes.get(id).cloned())?;
                    pane.user_title = self.chrome_ui.user_titles.get(id).cloned();
                    pane.pending_terminal_start = matches!(pane.kind, CardKind::Terminal(_))
                        && (self.durable.pending_terminal_creations.contains(id)
                            || pending.contains(id));
                    if let Scope::Worktree { path, .. } = scope {
                        pane.scope = Some(path.clone());
                    }
                    Some(pane)
                })
                .collect();
            panes.insert(scope.clone(), projected);
        }
        LocalViews {
            scopes,
            cards: members,
            panes,
        }
    }

    fn pane_transfer_source(&self, card: CardId) -> Option<(&View, &Path)> {
        if self.focused_repository_is_saving()
            || matches!(self.cards.get(&card), Some(CardRuntime::Repository(runtime)) if runtime.is_saving())
        {
            return None;
        }
        if !self
            .card_state(card)
            .is_some_and(|state| state.existence.is_open() && !state.kind.is_utility())
            || self
                .shared_views
                .requested_transfers
                .values()
                .any(|pending| *pending == card)
            || self
                .shared_views
                .pending
                .iter()
                .any(|(_, _, change)| matches!(change, Change::Transfer { id, .. } if *id == card))
        {
            return None;
        }
        let source = self
            .workspace_model
            .views
            .iter()
            .find(|view| view.panes.iter().any(|pane| pane.id == card))?;
        if self.shared_views.blocked.contains(&source.scope) {
            return None;
        }
        let repository = match &source.scope {
            Scope::Channel(id) => &self.workspace_model.channel(*id)?.repository,
            Scope::Worktree { repository, .. } => repository,
        };
        Some((source, repository))
    }

    fn pane_move_destination_allowed(
        &self,
        source: &Scope,
        repository: &Path,
        destination: &crate::workspace_model::ModelChannel,
    ) -> bool {
        *source != Scope::Channel(destination.id)
            && repository == destination.repository.as_path()
            && !self
                .shared_views
                .blocked
                .contains(&Scope::Channel(destination.id))
            && !self.channel_ui.is_deleting(destination.id)
            && !self.channel_ui.was_deleted(destination.id)
    }

    fn pane_channel_transfer(
        &self,
        card: CardId,
        channel: crate::model::ChannelId,
    ) -> Option<(Scope, u64)> {
        let (source, repository) = self.pane_transfer_source(card)?;
        let destination = self.workspace_model.channel(channel)?;
        self.pane_move_destination_allowed(&source.scope, repository, destination)
            .then(|| (source.scope.clone(), source.revision))
    }

    pub(super) fn pane_has_move_destination(&self, card: CardId) -> bool {
        self.pane_transfer_source(card)
            .is_some_and(|(source, repository)| {
                self.workspace_model.channels.iter().any(|channel| {
                    self.pane_move_destination_allowed(&source.scope, repository, channel)
                })
            })
    }

    /// Authoritative channel metadata includes destinations this window has
    /// never opened. The pane can originate in either kind of shared view.
    pub(super) fn pane_move_destinations(
        &self,
        card: CardId,
    ) -> Vec<&crate::workspace_model::ModelChannel> {
        let Some((source, repository)) = self.pane_transfer_source(card) else {
            return Vec::new();
        };
        let mut channels: Vec<_> = self
            .workspace_model
            .channels
            .iter()
            .filter(|channel| {
                self.pane_move_destination_allowed(&source.scope, repository, channel)
            })
            .collect();
        channels.sort_by_key(|channel| (std::cmp::Reverse(channel.ordinal), channel.id));
        channels
    }

    pub(super) fn pane_move_destination_label<'a>(
        &self,
        channel: &'a crate::workspace_model::ModelChannel,
    ) -> Cow<'a, str> {
        let title = if channel.title.is_empty() {
            "New channel"
        } else {
            &channel.title
        };
        if self.workspace_model.channels.iter().any(|other| {
            other.id != channel.id
                && other.repository == channel.repository
                && other.title == channel.title
        }) {
            Cow::Owned(format!("{title} · {}", channel.ordinal))
        } else {
            Cow::Borrowed(title)
        }
    }

    pub(super) fn pane_transfer_pending(&self, card: CardId) -> bool {
        self.shared_views
            .requested_transfers
            .values()
            .any(|pending| *pending == card)
    }

    pub(super) fn can_move_pane_to_channel(
        &self,
        card: CardId,
        channel: crate::model::ChannelId,
    ) -> bool {
        self.pane_channel_transfer(card, channel).is_some()
    }

    pub(super) fn move_pane_to_channel(
        &mut self,
        card: CardId,
        channel: crate::model::ChannelId,
    ) -> Task<Message> {
        if self.workspace_inputs_blocked() {
            return Task::none();
        }
        let Some((scope, expected_revision)) = self.pane_channel_transfer(card, channel) else {
            self.status = Some(TransientNotice::warning(
                "Choose another channel in the pane's project",
            ));
            return Task::none();
        };
        let operation = uuid::Uuid::new_v4();
        let change = Change::Transfer {
            id: card,
            destination: Scope::Channel(channel),
            before: None,
        };
        self.shared_views
            .requested_transfers
            .insert(operation, card);
        if let Err(error) = self.workspace_model_tx.try_send(ModelEdit::ChangeView {
            scope,
            expected_revision,
            operation,
            change,
        }) {
            self.reject_pane_transfer(operation, format!("Could not queue the move: {error}"));
        }
        // Runtime and local placement remain authoritative until the daemon
        // acknowledges the atomic transfer. Reconciliation retains the actor.
        Task::none()
    }

    pub(super) fn reject_pane_transfer(&mut self, operation: uuid::Uuid, error: String) {
        self.shared_views.refused(operation);
        self.status = Some(TransientNotice::warning(format!(
            "Could not move pane: {error}"
        )));
    }

    pub(super) fn publish_view_changes(&mut self) {
        if self.shared_views.applying
            || self.workspace_model.identity == crate::workspace_model::NO_REGISTRY
        {
            return;
        }
        let LocalViews {
            scopes,
            cards,
            mut panes,
        } = self.local_views();
        let previous_owners = self
            .shared_views
            .baseline
            .iter()
            .flat_map(|(scope, panes)| panes.iter().map(|pane| (pane.id, scope.clone())))
            .collect::<HashMap<_, _>>();
        for (destination, desired) in &panes {
            for (index, pane) in desired.iter().enumerate() {
                if let Some(source) = previous_owners
                    .get(&pane.id)
                    .cloned()
                    .filter(|source| source != destination)
                {
                    if !self.shared_views.initialized.contains(&source)
                        || self.shared_views.blocked.contains(&source)
                        || self.shared_views.blocked.contains(destination)
                    {
                        continue;
                    }
                    let before = desired.get(index + 1).map(|pane| pane.id);
                    let operation = uuid::Uuid::new_v4();
                    let change = Change::Transfer {
                        id: pane.id,
                        destination: destination.clone(),
                        before,
                    };
                    let expected_revision = self
                        .workspace_model
                        .views
                        .iter()
                        .find(|view| view.scope == source)
                        .map(|view| view.revision)
                        .unwrap_or(0);
                    self.shared_views
                        .pending
                        .push((operation, source.clone(), change.clone()));
                    self.publish_model_edit(ModelEdit::ChangeView {
                        scope: source.clone(),
                        expected_revision,
                        operation,
                        change,
                    });
                    if let Some(previous) = self.shared_views.baseline.get_mut(&source) {
                        previous.retain(|item| item.id != pane.id);
                    }
                    let baseline = self
                        .shared_views
                        .baseline
                        .entry(destination.clone())
                        .or_default();
                    baseline.insert(index.min(baseline.len()), pane.clone());
                }
            }
        }
        for scope in scopes {
            if self.shared_views.blocked.contains(&scope) {
                continue;
            }
            if matches!(&scope, Scope::Channel(id) if self.channel_ui.was_deleted(*id) || self.channel_ui.is_deleting(*id))
            {
                continue;
            }
            let panes = panes.remove(&scope).unwrap_or_default();
            if !self.shared_views.initialized.contains(&scope) {
                if self.shared_views.seeding.insert(scope.clone()) {
                    // Only close IDs belonging to this host view. Other views'
                    // tombstones and utility panes are unrelated client state.
                    let closed = self
                        .workspace_model
                        .views
                        .iter()
                        .find(|view| view.scope == scope)
                        .map(|view| {
                            view.panes
                                .iter()
                                .filter_map(|pane| {
                                    self.card_state(pane.id)
                                        .filter(|card| !card.existence.is_open())
                                        .map(|_| pane.id)
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    // Explicit creations replay as stable Insert operations
                    // after the seed acknowledgment, preserving their anchor.
                    // They must not migrate as append-only legacy panes.
                    let legacy = panes
                        .into_iter()
                        .filter(|pane| !self.durable.pending_terminal_creations.contains(&pane.id))
                        .collect::<Vec<_>>();
                    self.publish_model_edit(ModelEdit::SeedView {
                        scope: scope.clone(),
                        panes: legacy.clone(),
                        closed,
                    });
                    self.shared_views.baseline.insert(scope.clone(), legacy);
                }
                continue;
            }
            let previous = self
                .shared_views
                .baseline
                .get(&scope)
                .cloned()
                .unwrap_or_default();
            let revision = self
                .workspace_model
                .views
                .iter()
                .find(|view| view.scope == scope)
                .map(|view| view.revision)
                .unwrap_or(0);
            for change in views::changes(&previous, &panes) {
                let operation = match &change {
                    Change::Insert { pane, .. }
                        if self.durable.pending_terminal_creations.contains(&pane.id) =>
                    {
                        pane.id.as_uuid()
                    }
                    _ => uuid::Uuid::new_v4(),
                };
                self.shared_views
                    .pending
                    .push((operation, scope.clone(), change.clone()));
                self.publish_model_edit(ModelEdit::ChangeView {
                    scope: scope.clone(),
                    expected_revision: revision,
                    operation,
                    change,
                });
            }
            self.shared_views.baseline.insert(scope.clone(), panes);
            let defaults = cards[&scope]
                .iter()
                .copied()
                .filter(|id| self.shared_views.defaults.contains_key(id))
                .collect::<Vec<_>>();
            for id in defaults {
                if self.shared_views.default_requests.insert(id) {
                    if let Some(mut pane) = self.card_state(id).and_then(|card| {
                        Pane::from_card(card, self.durable.card_scopes.get(&id).cloned())
                    }) {
                        let operation = uuid::Uuid::new_v4();
                        pane.pending_terminal_start = true;
                        let change = Change::InsertIfEmpty { pane };
                        self.shared_views
                            .pending
                            .push((operation, scope.clone(), change.clone()));
                        self.publish_model_edit(ModelEdit::ChangeView {
                            scope: scope.clone(),
                            expected_revision: revision,
                            operation,
                            change,
                        });
                    }
                }
            }
        }
    }

    pub(super) fn acknowledge_view_edit(&mut self, scope: Scope, operation: Option<uuid::Uuid>) {
        if let Some(operation) = operation {
            self.shared_views.requested_transfers.remove(&operation);
            self.shared_views
                .pending
                .retain(|(id, _, _)| *id != operation);
        } else {
            self.shared_views.seeding.remove(&scope);
            self.shared_views.initialized.insert(scope);
            // Preserve edits made while the seed snapshot was in flight.
            self.publish_view_changes();
        }
    }

    pub(super) fn reconcile_shared_views(&mut self) -> Task<Message> {
        self.shared_views.prune_restoration_requests(&self.durable);
        self.shared_views.applying = true;
        let mut tasks = Vec::new();
        let mut views = self.workspace_model.views.clone();
        for (_, source, change) in &self.shared_views.pending {
            if let Change::Transfer {
                id,
                destination,
                before,
            } = change
            {
                if !views.iter().any(|view| &view.scope == destination) {
                    views.push(View::empty(destination.clone()));
                }
                let _ = views::transfer(&mut views, source, *id, destination, *before);
            }
        }
        for view in &mut views {
            for (_, scope, change) in &self.shared_views.pending {
                if *scope == view.scope && !matches!(change, Change::Transfer { .. }) {
                    let _ = view.apply(change);
                }
            }
        }
        let owned = views
            .iter()
            .flat_map(|view| view.panes.iter().map(|pane| pane.id))
            .collect::<HashSet<_>>();
        self.prepare_shared_aliases(&views, &owned);
        // Root discovery is immutable during this projection. Keep only owned
        // paths across mutable view updates, and refresh the panes each view
        // changes before visiting the next one.
        let mut local_worktrees = {
            let roots = self.indexed_worktree_roots();
            self.durable
                .cards
                .iter()
                .filter(|card| card.existence.is_open())
                .filter_map(|card| {
                    self.unchanneled_card_worktree_indexed(card, &roots)
                        .map(|path| (card.id, path.to_owned()))
                })
                .collect::<HashMap<_, _>>()
        };
        let mut projected_scopes = Vec::new();
        for mut view in views {
            if self.shared_views.blocked.contains(&view.scope) {
                continue;
            }
            if !self.shared_views.initialized.contains(&view.scope)
                || self.shared_views.seeding.contains(&view.scope)
            {
                continue;
            }
            for (_, scope, change) in &self.shared_views.pending {
                if *scope == view.scope && !matches!(change, Change::Transfer { .. }) {
                    if let Err(error) = view.apply(change) {
                        self.status = Some(TransientNotice::error(error));
                    }
                }
            }
            let current = match &view.scope {
                Scope::Channel(_) => self.local_view_cards(&view.scope),
                Scope::Worktree { path, .. } => {
                    let channel_cards = self
                        .durable
                        .channels
                        .iter()
                        .flat_map(|channel| channel.cards.iter().copied())
                        .collect::<HashSet<_>>();
                    self.durable
                        .cards
                        .iter()
                        .filter(|card| {
                            card.existence.is_open()
                                && !card.kind.is_utility()
                                && !channel_cards.contains(&card.id)
                                && local_worktrees.get(&card.id) == Some(path)
                        })
                        .map(|card| card.id)
                        .collect()
                }
            };
            tasks.push(self.project_shared_view_with_cards(&view, &owned, current));
            if !view.panes.is_empty() {
                let roots = self.indexed_worktree_roots();
                for pane in &view.panes {
                    if let Some(path) = self
                        .card_state(pane.id)
                        .and_then(|card| self.unchanneled_card_worktree_indexed(card, &roots))
                    {
                        local_worktrees.insert(pane.id, path.to_owned());
                    } else {
                        local_worktrees.remove(&pane.id);
                    }
                }
            }
            projected_scopes.push(view.scope);
        }
        // Capture the completed projection once. A later view may move an
        // owned pane out of an earlier one, so baselines must describe the
        // settled workspace rather than an intermediate projection.
        let mut projected = self.local_views();
        for scope in projected_scopes {
            self.shared_views.baseline.insert(
                scope.clone(),
                projected.panes.remove(&scope).unwrap_or_default(),
            );
        }
        self.shared_views.applying = false;
        // Adopt all canonical descriptors before an ordered migration checkpoint.
        self.persist();
        if let Some(path) = self.shared_views.deferred_default.take() {
            if self.active_workspace.as_ref() == Some(&path) && self.workspace.cards().is_empty() {
                tasks.push(self.add_default_pane(path, settings::DefaultPane::Terminal));
            }
        }
        Task::batch(tasks)
    }

    fn prepare_shared_aliases(&mut self, views: &[View], owned: &HashSet<CardId>) {
        self.shared_views.blocked.clear();
        // Decide which whole views can project before retiring any local alias.
        // Otherwise a later blocked pane could leave an earlier adoption without
        // the canonical descriptor required by its persistence checkpoint.
        let mut adoptions = Vec::new();
        for view in views {
            for shared in &view.panes {
                let aliases = self
                    .durable
                    .cards
                    .iter()
                    .filter(|card| {
                        card.existence.is_open()
                            && card.id != shared.id
                            && !owned.contains(&card.id)
                            && views::same_resource(&card.kind, &shared.kind)
                    })
                    .map(|card| card.id)
                    .collect::<Vec<_>>();
                for alias in aliases {
                    let ready = self.shared_views.initialized.contains(&view.scope)
                        && !self.shared_views.seeding.contains(&view.scope)
                        && match &view.scope {
                            Scope::Channel(id) => self
                                .durable
                                .channels
                                .iter()
                                .any(|channel| channel.id == *id),
                            Scope::Worktree { repository, .. } => self
                                .sidebar_model
                                .projects
                                .iter()
                                .any(|project| &project.repository == repository),
                        };
                    if !ready {
                        for scope in self.local_view_scopes() {
                            if scope != view.scope && self.local_view_cards(&scope).contains(&alias)
                            {
                                self.shared_views.blocked.insert(scope);
                            }
                        }
                        continue;
                    }
                    let local_work = self.shared_chat_has_local_work(alias);
                    let can_adopt = if matches!(shared.kind, CardKind::Terminal(_)) {
                        self.can_rekey_shared_terminal_draft(alias, shared.id)
                    } else {
                        !local_work
                    };
                    if !can_adopt {
                        self.shared_views.blocked.insert(view.scope.clone());
                        for scope in self.local_view_scopes() {
                            if self.local_view_cards(&scope).contains(&alias) {
                                self.shared_views.blocked.insert(scope);
                            }
                        }
                        self.status = Some(TransientNotice::warning(
                            "A shared pane has a new identity. Finish or discard its local draft before adopting it.",
                        ));
                        continue;
                    }
                    adoptions.push((view.scope.clone(), alias, shared.clone()));
                }
            }
        }
        for (scope, alias, shared) in adoptions {
            if self.shared_views.blocked.contains(&scope) {
                continue;
            }
            if let CardKind::Terminal(session) = &shared.kind {
                if !self.rekey_shared_terminal_draft(alias, shared.id) {
                    continue;
                }
                self.shared_views
                    .adoptions
                    .push(crate::persistence::TerminalPaneAdoption {
                        alias,
                        canonical: shared.id,
                        terminal: session.id,
                    });
            }
            self.detach_shared_pane(alias);
            for channel in &mut Arc::make_mut(&mut self.durable).channels {
                channel.cards.retain(|id| *id != alias);
            }
        }
    }

    fn prepare_terminal_restoration(&mut self, pane: &Pane) {
        let CardKind::Terminal(terminal) = &pane.kind else {
            return;
        };
        let already_projected = self.durable.cards.iter().any(|card| {
            card.id == pane.id
                && card.existence.is_open()
                && matches!(&card.kind, CardKind::Terminal(session) if session.id == terminal.id)
        });
        if already_projected {
            if self.shared_views.restoration_requests.get(&pane.id) == Some(&terminal.id) {
                return;
            }
        } else {
            // A new reopen must resolve ownership again. The writer retains
            // queued intents across transient failures independently of this
            // projection-local deduplication.
            self.shared_views.restoration_requests.remove(&pane.id);
        }
        if self.session.is_none() || !self.durable.cards.iter().any(|source| {
            source.id != pane.id
                && !source.existence.is_open()
                && matches!(&source.kind, CardKind::Terminal(session) if session.id == terminal.id)
        }) {
            return;
        }
        // Resolving the recipe's actual archived owner belongs to the ordered
        // writer checkpoint. Even a contended autosave must not block native input.
        let restoration = crate::persistence::TerminalPaneRestoration {
            canonical: pane.id,
            terminal: terminal.id,
        };
        if !self.shared_views.restorations.contains(&restoration) {
            self.shared_views.restorations.push(restoration);
        }
        self.shared_views
            .restoration_requests
            .insert(pane.id, terminal.id);
    }

    fn project_shared_view(&mut self, view: &View, owned: &HashSet<CardId>) -> Task<Message> {
        let current = self.local_view_cards(&view.scope);
        self.project_shared_view_with_cards(view, owned, current)
    }

    fn project_shared_view_with_cards(
        &mut self,
        view: &View,
        owned: &HashSet<CardId>,
        current: Vec<CardId>,
    ) -> Task<Message> {
        let index = match &view.scope {
            Scope::Channel(id) => {
                let Some(index) = self
                    .durable
                    .channels
                    .iter()
                    .position(|channel| channel.id == *id)
                else {
                    return Task::none();
                };
                Some(index)
            }
            Scope::Worktree { repository, .. } => {
                if !self
                    .sidebar_model
                    .projects
                    .iter()
                    .any(|project| &project.repository == repository)
                {
                    return Task::none();
                }
                None
            }
        };
        let active = match &view.scope {
            Scope::Channel(id) => self.durable.active_channel == Some(*id),
            Scope::Worktree { path, .. } => {
                self.durable.active_channel.is_none()
                    && self.active_workspace.as_ref() == Some(path)
            }
        };
        // Historical empty scopes have no descriptors or strip to project.
        // The active scope still settles its strip, including transferred panes.
        if !active && current.is_empty() && view.panes.is_empty() {
            return Task::none();
        }
        let mut tasks = Vec::new();
        for id in current.iter().copied() {
            if self.chat_is_unused_default(id) && view.panes.is_empty() {
                continue;
            }
            if owned.contains(&id)
                || self
                    .card_state(id)
                    .is_some_and(|card| card.kind.is_utility())
            {
                continue;
            }
            if matches!(self.cards.get(&id), Some(CardRuntime::Repository(runtime)) if runtime.is_dirty() || runtime.is_saving())
                || self.shared_chat_has_local_work(id)
            {
                self.retain_local_pane(id);
                self.status = Some(TransientNotice::warning(
                    "A pane was closed in another client. Its unsaved edits are retained here; save or discard them before closing it.",
                ));
                continue;
            }
            self.detach_shared_pane(id);
        }
        for shared in &view.panes {
            self.prepare_terminal_restoration(shared);
            let mut projected = shared.clone();
            if let Scope::Worktree { path, .. } = &view.scope {
                projected.scope = Some(path.clone());
                self.worktree_owners.insert(projected.id, path.clone());
                for channel in &mut Arc::make_mut(&mut self.durable).channels {
                    channel.cards.retain(|id| *id != projected.id);
                }
            }

            if let CardKind::Repository(repository) = &mut projected.kind {
                if let Some(project) =
                    self.project_for_repository(&repository.repository)
                        .or_else(|| {
                            self.sidebar_model
                                .worktree_at(&repository.worktree)
                                .map(|key| key.project_id)
                        })
                {
                    repository.project = project;
                }
            }
            if let CardKind::Browser(browser) = &mut projected.kind {
                if let Some(CardKind::Browser(local)) =
                    self.card_state(projected.id).map(|card| &card.kind)
                {
                    if local.current_url == browser.current_url {
                        browser.scroll_x = local.scroll_x;
                        browser.scroll_y = local.scroll_y;
                    }
                }
            }
            let pane = &projected;
            let accepted_default = self
                .workspace_model
                .views
                .iter()
                .any(|view| view.panes.iter().any(|shared| shared.id == pane.id));
            if accepted_default {
                let automatic = self.shared_views.defaults.remove(&pane.id);
                self.shared_views.default_requests.remove(&pane.id);
                let explicit = Arc::make_mut(&mut self.durable)
                    .pending_terminal_creations
                    .remove(&pane.id);
                if automatic.is_some() || explicit {
                    if let CardKind::Terminal(session) = &pane.kind {
                        self.shared_views.attached.insert(pane.id);
                        tasks.push(self.start_terminal(pane.id, session.clone(), true, false));
                    }
                }
            }
            self.shared_views.attached.insert(pane.id);
            match &pane.user_title {
                Some(title) => {
                    self.chrome_ui.user_titles.insert(pane.id, title.clone());
                }
                None => {
                    self.chrome_ui.user_titles.remove(&pane.id);
                }
            }
            let existing = self.card_state(pane.id).cloned();
            // A restored shared terminal may have waited for the registry
            // while lifecycle hydration was admitted. Once its authoritative
            // descriptor arrives, admit that exact identity again.
            let no_start = !self
                .terminal_starts
                .lock()
                .expect("terminal starts lock")
                .contains_key(&pane.id);
            let waiting = matches!(self.cards.get(&pane.id), Some(CardRuntime::Loading))
                && existing
                    .as_ref()
                    .is_some_and(|card| matches!(card.residency, Residency::Hydrating { .. }));
            let failed_pending = pane.pending_terminal_start
                && matches!(self.cards.get(&pane.id), Some(CardRuntime::Failed(_)));
            if matches!(pane.kind, CardKind::Terminal(_)) && no_start && (waiting || failed_pending)
            {
                self.cards.insert(pane.id, CardRuntime::Loading);
                let exposure = existing
                    .as_ref()
                    .map_or(Exposure::Offscreen, |card| card.exposure);
                self.lifecycle
                    .register(pane.id, exposure, Residency::Cold, Instant::now());
                if let Some(card) = self.card_state_mut(pane.id) {
                    card.residency = Residency::Cold;
                }
            }
            match existing {
                Some(card) if card.existence.is_open() => {
                    if card.kind != pane.kind
                        && matches!(self.cards.get(&pane.id), Some(CardRuntime::Repository(runtime)) if runtime.is_dirty() || runtime.is_saving())
                    {
                        self.status = Some(TransientNotice::warning(
                            "Another client changed this pane. Save or discard its unsaved edits before adopting that change.",
                        ));
                        continue;
                    }
                    if card.kind != pane.kind {
                        // Runtime metadata can change, but an exact live actor
                        // remains attached unless the session identity changes.
                        let same_session = match (&card.kind, &pane.kind) {
                            (CardKind::Terminal(a), CardKind::Terminal(b)) => a.id == b.id,
                            (CardKind::Chat(a), CardKind::Chat(b)) => a.id == b.id,
                            (a, b) => a == b,
                        };
                        if !same_session {
                            self.cards.remove(&pane.id);
                            self.lifecycle.remove(pane.id);
                        }
                    }
                    let mut kind = pane.kind.clone();
                    preserve_observed_terminal_exit(&card, &mut kind);
                    if let Some(current) = self.card_state_mut(pane.id) {
                        current.title = pane.title.clone();
                        current.width_fraction = pane.width_fraction;
                        current.kind = kind;
                    }
                    if !self.cards.contains_key(&pane.id) {
                        tasks.push(self.attach_shared_pane(pane));
                    }
                }
                _ => {
                    let card = CardState {
                        id: pane.id,
                        generation: self.durable.generation,
                        title: pane.title.clone(),
                        width_fraction: pane.width_fraction,
                        existence: Existence::Open,
                        exposure: Exposure::Offscreen,
                        residency: Residency::Cold,
                        kind: pane.kind.clone(),
                    };
                    let durable = Arc::make_mut(&mut self.durable);
                    if let Some(existing) = durable.cards.iter_mut().find(|card| card.id == pane.id)
                    {
                        *existing = card;
                    } else {
                        durable.cards.push(card);
                    }
                    tasks.push(self.attach_shared_pane(pane));
                }
            }
            let durable = Arc::make_mut(&mut self.durable);
            match &pane.scope {
                Some(scope) => {
                    durable.card_scopes.insert(pane.id, scope.clone());
                }
                None => {
                    durable.card_scopes.remove(&pane.id);
                }
            }
        }
        let retained = current
            .into_iter()
            .filter(|id| {
                self.shared_views.retained.contains(id)
                    || (view.panes.is_empty() && self.chat_is_unused_default(*id))
                    || self
                        .card_state(*id)
                        .is_some_and(|card| card.kind.is_utility())
            })
            .collect::<Vec<_>>();
        let visible = view
            .panes
            .iter()
            .map(|pane| pane.id)
            .chain(retained)
            .collect::<Vec<_>>();
        if let Some(index) = index {
            let channel = &mut Arc::make_mut(&mut self.durable).channels[index];
            channel.cards = visible.clone();
            if !channel
                .focused
                .is_some_and(|id| channel.cards.contains(&id))
            {
                channel.focused = channel.cards.first().copied();
            }
        }
        let members = visible.iter().copied().collect::<HashSet<_>>();
        let states = self
            .durable
            .cards
            .iter()
            .filter(|card| members.contains(&card.id))
            .map(|card| (card.id, card.clone()))
            .collect::<HashMap<_, _>>();
        let mut ordered = visible.iter().filter_map(|id| states.get(id).cloned());
        for card in &mut Arc::make_mut(&mut self.durable).cards {
            if members.contains(&card.id) {
                if let Some(next) = ordered.next() {
                    *card = next;
                }
            }
        }
        if active {
            tasks.push(self.project_shared_strip(&view.panes, &visible));
        }
        if !self.shared_views.applying {
            self.persist();
        }
        Task::batch(tasks)
    }

    fn attach_shared_pane(&mut self, pane: &Pane) -> Task<Message> {
        self.cards.insert(pane.id, CardRuntime::Loading);
        self.lifecycle.register(
            pane.id,
            Exposure::Offscreen,
            Residency::Cold,
            Instant::now(),
        );
        let task = match &pane.kind {
            CardKind::Chat(session) => {
                return self.open_chat(pane.id, session.clone(), crate::chat::Intent::Restore);
            }
            CardKind::Repository(view) => self.attach_repository_pane(pane.id, view.clone()),
            CardKind::PullRequests(view) => self.attach_pull_request_pane(pane.id, view.clone()),
            kind @ (CardKind::Settings | CardKind::Accounts | CardKind::Automations) => {
                self.cards.insert(pane.id, CardRuntime::utility(kind));
                if matches!(kind, CardKind::Automations) {
                    self.update_automation_ui(crate::automation_ui::Action::Refresh)
                } else {
                    Task::none()
                }
            }
            #[cfg(not(feature = "integrated-browser"))]
            CardKind::Browser(_) => {
                self.cards.insert(pane.id, CardRuntime::Failed(
                    "This cagents build does not include the integrated browser. Open the saved URL in your system browser.".into(),
                ));
                Task::none()
            }
            _ => return Task::none(),
        };
        // Lightweight panes are ready immediately; terminal/browser engines
        // continue through their deferred lifecycle hydration above.
        self.lifecycle
            .register(pane.id, Exposure::Offscreen, Residency::Hot, Instant::now());
        if let Some(card) = self.card_state_mut(pane.id) {
            card.residency = Residency::Hot;
        }
        task
    }

    /// Passive removal detaches surfaces; it never calls a host close request.
    fn detach_shared_pane(&mut self, id: CardId) {
        self.shared_views.defaults.remove(&id);
        Arc::make_mut(&mut self.durable)
            .pending_terminal_creations
            .remove(&id);
        self.shared_views.default_requests.remove(&id);
        self.shared_chat_detach(id);
        self.retire_lsp_card(id);
        if let Some(runtime) = self.cards.remove(&id) {
            #[cfg(feature = "integrated-browser")]
            if let CardRuntime::Browser(browser) = runtime {
                self.begin_browser_teardown(browser.handle);
            }
            #[cfg(not(feature = "integrated-browser"))]
            drop(runtime);
        }
        self.lifecycle.remove(id);
        if self.workspace.card(id).is_some() {
            if self.workspace.request_close(id).is_ok() {
                let _ = self.workspace.commit_close(id);
            }
        }
        if let Some(card) = self.card_state_mut(id) {
            card.existence = super::project_ui::removed_existence("pane closed in another client");
        }
        // The strip handed this pane's focus to a neighbour above, with no
        // viewport update to tell the saved state.
        self.settle_focus();
        self.sidebar_state.clear_agent_activity(id);
    }

    fn project_shared_strip(&mut self, panes: &[Pane], visible: &[CardId]) -> Task<Message> {
        let camera = self.workspace.camera().offset();
        let focus = self.workspace.focused();
        let mut updates = Vec::new();
        let visible = visible.iter().copied().collect::<HashSet<_>>();
        let removed = self
            .workspace
            .order()
            .filter(|id| !visible.contains(id))
            .collect::<Vec<_>>();
        for id in removed {
            // A transfer removes only this surface, leaving the actor and its
            // local runtime available in the destination view.
            match self.cards.get_mut(&id) {
                Some(CardRuntime::Terminal(surface)) => {
                    let _ = surface.focus(false);
                    let _ = surface.hide();
                }
                Some(CardRuntime::Chat(runtime)) => {
                    let _ = runtime.set_visible(false);
                }
                #[cfg(feature = "integrated-browser")]
                Some(CardRuntime::Browser(browser)) => {
                    let _ = browser.handle.set_visible(false);
                }
                _ => {}
            }
            if let Some(state) = self.card_state_mut(id) {
                state.exposure = Exposure::Offscreen;
            }
            if self.workspace.request_close(id).is_ok() {
                if let Ok(update) = self.workspace.commit_close(id) {
                    updates.push(update);
                }
            }
        }
        for (index, pane) in panes.iter().enumerate() {
            if self.workspace.card(pane.id).is_none() {
                if let Ok(card) = WorkspaceCard::new(
                    pane.id,
                    pane.user_title.as_deref().unwrap_or_else(|| {
                        browser_host::presentation_title(&pane.kind, &pane.title)
                    }),
                    CardWidth::Fraction(pane.width_fraction),
                ) {
                    if let Ok(update) = self.workspace.add_card(
                        card,
                        index.min(self.workspace.cards().len()),
                        false,
                    ) {
                        updates.push(update);
                    }
                }
            } else {
                let _ = self.workspace.rename_card(
                    pane.id,
                    pane.user_title
                        .as_deref()
                        .unwrap_or_else(|| {
                            browser_host::presentation_title(&pane.kind, &pane.title)
                        })
                        .to_owned(),
                );
                if self
                    .workspace
                    .card(pane.id)
                    .is_some_and(|card| card.width != CardWidth::Fraction(pane.width_fraction))
                {
                    if let Ok(update) = self
                        .workspace
                        .set_card_width(pane.id, CardWidth::Fraction(pane.width_fraction))
                    {
                        updates.push(update);
                    }
                }
                if self.workspace.order().nth(index) != Some(pane.id) {
                    if let Ok(update) = self.workspace.reorder(pane.id, index) {
                        updates.push(update);
                    }
                }
            }
        }
        if let Some(focus) = focus.filter(|id| self.workspace.card(*id).is_some()) {
            let _ = self.workspace.focus_without_scrolling(focus);
        }
        self.workspace.record_programmatic_scroll(camera);
        let lifecycle = self.lifecycle_sweep();
        Task::batch(
            updates
                .into_iter()
                .map(|update| self.apply_viewport(update, false, false))
                .chain(std::iter::once(lifecycle)),
        )
    }
}

// A layout snapshot can predate a runtime exit. Missing recipe metadata cannot
// revoke a settled exit for that exact terminal; new identities and live host
// proofs still adopt normally, as do the incoming launch and other metadata.
fn preserve_observed_terminal_exit(card: &CardState, incoming: &mut CardKind) {
    let Some(observed) = ended_terminal(Some(card)) else {
        return;
    };
    let CardKind::Terminal(incoming) = incoming else {
        return;
    };
    if observed.id != incoming.id {
        return;
    }
    match (&observed.continuity, &mut incoming.continuity) {
        (
            TerminalContinuity::RelaunchAvailable {
                last_exit: Some(exit),
                ..
            }
            | TerminalContinuity::Exited { last_exit: exit },
            TerminalContinuity::RelaunchAvailable { last_exit, .. },
        ) if last_exit.is_none() => *last_exit = Some(exit.clone()),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        BrowserId, BrowserRestoreFidelity, BrowserSession, ChannelId, ProjectChannel, ProjectId,
    };
    fn fixture() -> (Cagents, ChannelId, ChannelId, Pane) {
        let mut app = super::super::tests::context_inspector_test_app(false);
        let a = ChannelId::new();
        let b = ChannelId::new();
        let project = ProjectId::new();
        let pane = Pane {
            id: CardId::new(),
            title: "shared".into(),
            user_title: None,
            pending_terminal_start: false,
            width_fraction: 0.5,
            scope: None,
            kind: CardKind::Browser(BrowserSession {
                id: BrowserId::new(),
                current_url: "https://example.com".parse().unwrap(),
                history: vec![],
                history_index: 0,
                scroll_x: 0.,
                scroll_y: 0.,
                fidelity: BrowserRestoreFidelity::UrlOnly,
            }),
        };
        let durable = Arc::make_mut(&mut app.durable);
        for id in [a, b] {
            durable.channels.push(ProjectChannel {
                id,
                project_id: project,
                title: "view".into(),
                title_source: Default::default(),
                ordinal: 1,
                shared: true,
                cards: vec![],
                focused: None,
            });
        }
        app.workspace_model.identity = uuid::Uuid::new_v4();
        app.shared_views
            .initialized
            .extend([Scope::Channel(a), Scope::Channel(b)]);
        (app, a, b, pane)
    }

    fn channel_move_fixture() -> (Cagents, ChannelId, ChannelId, Pane) {
        let (mut app, source, destination, pane) = fixture();
        app.workspace_model.channels = [source, destination]
            .into_iter()
            .map(|id| crate::workspace_model::ModelChannel {
                id,
                repository: PathBuf::from("/repo/.git"),
                title: "Channel".into(),
                title_source: Default::default(),
                ordinal: 1,
            })
            .collect();
        let mut source_view = View::empty(source);
        source_view.revision = 17;
        source_view.panes.push(pane.clone());
        app.workspace_model.views = vec![source_view, View::empty(destination)];
        Arc::make_mut(&mut app.durable).active_channel = Some(source);
        drop(app.reconcile_shared_views());
        (app, source, destination, pane)
    }

    #[test]
    fn active_repository_save_blocks_channel_commands_picker_and_direct_moves() {
        let (mut app, source, destination, pane) = channel_move_fixture();
        let (tx, rx) = async_channel::unbounded();
        app.workspace_model_tx = tx;
        assert!(app.pane_has_move_destination(pane.id));
        let (runtime, _) = RepositoryRuntime::post_persist_failure_fixture(pane.id);
        app.cards
            .insert(pane.id, CardRuntime::Repository(Box::new(runtime)));
        assert!(app.focused_repository_is_saving());
        assert!(!app.pane_has_move_destination(pane.id));
        assert!(app.pane_move_destinations(pane.id).is_empty());
        assert!(!app.command_available(workspace_widget::GlobalShortcut::MovePaneToChannel));
        drop(app.move_pane_to_channel(pane.id, destination));
        drop(app.update(Message::MovePaneToChannel {
            card: pane.id,
            channel: destination,
        }));
        drop(app.update(Message::PaneDropToChannel {
            generation: app.workspace_view_generation,
            card: pane.id,
            channel: destination,
        }));
        drop(app.update_palette(super::super::palette_ui::Action::MovePane(pane.id)));
        assert!(!app.palette.is_open());
        assert!(rx.try_recv().is_err());
        assert!(!app.pane_transfer_pending(pane.id));
        assert_eq!(app.durable.active_channel, Some(source));
        assert!(
            app.durable
                .channels
                .iter()
                .find(|channel| channel.id == source)
                .unwrap()
                .cards
                .contains(&pane.id)
        );
    }

    #[test]
    fn move_picker_ranks_exact_unvisited_destinations_before_loose_matches() {
        let (mut app, source, loose, pane) = channel_move_fixture();
        let exact = ChannelId::new();
        let prefix = ChannelId::new();
        let repository = PathBuf::from("/repo/.git");
        app.workspace_model
            .channels
            .iter_mut()
            .find(|channel| channel.id == loose)
            .unwrap()
            .title = "r e f a c t o r".into();
        for (id, title, ordinal) in [(exact, "refactor", 1), (prefix, "refactor ui", 2)] {
            app.workspace_model
                .channels
                .push(crate::workspace_model::ModelChannel {
                    id,
                    repository: repository.clone(),
                    title: title.into(),
                    title_source: Default::default(),
                    ordinal,
                });
        }
        let (tx, rx) = async_channel::unbounded();
        app.workspace_model_tx = tx;
        drop(app.update_palette(super::super::palette_ui::Action::MovePane(pane.id)));
        drop(app.update_palette(super::super::palette_ui::Action::Query("refactor".into())));
        drop(app.update_palette(super::super::palette_ui::Action::Submit));
        assert!(
            matches!(rx.try_recv().unwrap(), ModelEdit::ChangeView { scope: Scope::Channel(from), change: Change::Transfer { destination: Scope::Channel(to), .. }, .. } if from == source && to == exact)
        );
        assert!(
            app.durable
                .channels
                .iter()
                .find(|channel| channel.id == source)
                .unwrap()
                .cards
                .contains(&pane.id)
        );
    }

    #[test]
    fn channel_move_waits_for_host_acknowledgment_and_preserves_the_pane() {
        let (mut app, source, destination, pane) = channel_move_fixture();
        let (tx, rx) = async_channel::unbounded();
        app.workspace_model_tx = tx;
        let kind = app.card_state(pane.id).unwrap().kind.clone();
        let runtime = app.cards.get(&pane.id).unwrap() as *const CardRuntime;
        drop(app.move_pane_to_channel(pane.id, destination));
        let ModelEdit::ChangeView {
            scope,
            expected_revision,
            operation,
            change,
        } = rx.try_recv().unwrap()
        else {
            panic!("a transfer request");
        };
        assert_eq!(scope, Scope::Channel(source));
        assert_eq!(expected_revision, 17);
        assert!(
            matches!(change, Change::Transfer { id, destination: Scope::Channel(to), before: None }
            if id == pane.id && to == destination)
        );
        assert!(
            !app.can_move_pane_to_channel(pane.id, destination),
            "duplicate pending move is disabled"
        );
        drop(app.begin_card_close(pane.id));
        assert!(
            app.card_state(pane.id).unwrap().existence.is_open(),
            "closing against the old scope must wait for the transfer acknowledgment"
        );
        assert!(
            app.status
                .as_ref()
                .unwrap()
                .message
                .contains("moving to a channel")
        );
        assert!(
            rx.try_recv().is_err(),
            "a pending move must not publish a stale-scope removal"
        );
        drop(app.reconcile_shared_views());
        assert!(
            app.durable
                .channels
                .iter()
                .find(|c| c.id == source)
                .unwrap()
                .cards
                .contains(&pane.id)
        );
        assert!(
            app.durable
                .channels
                .iter()
                .find(|c| c.id == destination)
                .unwrap()
                .cards
                .is_empty()
        );
        views::transfer(
            &mut app.workspace_model.views,
            &Scope::Channel(source),
            pane.id,
            &Scope::Channel(destination),
            None,
        )
        .unwrap();
        app.acknowledge_view_edit(scope, Some(operation));
        drop(app.reconcile_shared_views());
        assert_eq!(app.card_state(pane.id).unwrap().kind, kind);
        assert_eq!(
            app.cards.get(&pane.id).unwrap() as *const CardRuntime,
            runtime
        );
        assert!(
            app.durable
                .channels
                .iter()
                .find(|c| c.id == destination)
                .unwrap()
                .cards
                .contains(&pane.id)
        );
    }

    #[test]
    fn channel_move_rejects_other_projects_and_reports_host_refusal_without_rehoming() {
        let (mut app, source, destination, pane) = channel_move_fixture();
        let (tx, rx) = async_channel::unbounded();
        app.workspace_model_tx = tx;
        app.workspace_model
            .channels
            .iter_mut()
            .find(|c| c.id == destination)
            .unwrap()
            .repository = PathBuf::from("/other/.git");
        assert!(!app.can_move_pane_to_channel(pane.id, destination));
        drop(app.move_pane_to_channel(pane.id, destination));
        assert!(rx.try_recv().is_err());
        assert!(!app.can_move_pane_to_channel(CardId::new(), destination));
        app.workspace_model
            .channels
            .iter_mut()
            .find(|c| c.id == destination)
            .unwrap()
            .repository = PathBuf::from("/repo/.git");
        drop(app.move_pane_to_channel(pane.id, destination));
        let ModelEdit::ChangeView { operation, .. } = rx.try_recv().unwrap() else {
            panic!("transfer");
        };
        drop(app.apply_workspace_model_update(
            super::super::workspace_model_sync::ModelUpdate::PaneTransferFailed {
                operation,
                error: "destination no longer exists".into(),
            },
        ));
        assert!(app.shared_views.requested_transfers.is_empty());
        assert!(
            app.status
                .as_ref()
                .unwrap()
                .message
                .contains("destination no longer exists")
        );
        assert!(
            app.durable
                .channels
                .iter()
                .find(|c| c.id == source)
                .unwrap()
                .cards
                .contains(&pane.id)
        );
        assert!(app.can_move_pane_to_channel(pane.id, destination));
    }

    #[test]
    fn refused_optimistic_transfer_restores_membership_without_erasing_other_intentions() {
        let (mut app, source, destination, pane) = channel_move_fixture();
        let refused = uuid::Uuid::new_v4();
        let rename = uuid::Uuid::new_v4();
        let mut renamed = pane.clone();
        renamed.title = "Keep my pending rename".into();
        app.shared_views.pending.push((
            refused,
            Scope::Channel(source),
            Change::Transfer {
                id: pane.id,
                destination: Scope::Channel(destination),
                before: None,
            },
        ));
        app.shared_views.pending.push((
            rename,
            Scope::Channel(source),
            Change::Update {
                before: pane.clone(),
                pane: renamed,
            },
        ));
        drop(app.reconcile_shared_views());
        assert!(
            app.durable
                .channels
                .iter()
                .find(|channel| channel.id == destination)
                .unwrap()
                .cards
                .contains(&pane.id)
        );
        drop(app.apply_workspace_model_update(
            super::super::workspace_model_sync::ModelUpdate::ViewRefused {
                scope: Scope::Channel(source),
                operation: refused,
                error: "destination was removed".into(),
            },
        ));
        assert_eq!(app.shared_views.pending.len(), 1);
        assert_eq!(app.shared_views.pending[0].0, rename);
        assert!(
            app.durable
                .channels
                .iter()
                .find(|channel| channel.id == source)
                .unwrap()
                .cards
                .contains(&pane.id)
        );
        assert!(
            app.durable
                .channels
                .iter()
                .find(|channel| channel.id == destination)
                .unwrap()
                .cards
                .is_empty()
        );
        assert_eq!(
            app.card_state(pane.id).unwrap().title,
            "Keep my pending rename"
        );
        assert!(app.card_state(pane.id).unwrap().existence.is_open());
    }

    #[test]
    fn worktree_panes_can_choose_authoritative_unvisited_channels() {
        let (mut app, source, destination, pane) = channel_move_fixture();
        let worktree = Scope::Worktree {
            repository: PathBuf::from("/repo/.git"),
            path: PathBuf::from("/repo/worktree"),
        };
        app.workspace_model
            .views
            .iter_mut()
            .find(|view| view.scope == Scope::Channel(source))
            .unwrap()
            .scope = worktree.clone();
        let durable = Arc::make_mut(&mut app.durable);
        durable.active_channel = None;
        durable.channels.clear();
        durable
            .card_scopes
            .insert(pane.id, PathBuf::from("/repo/worktree"));
        let destinations = app.pane_move_destinations(pane.id);
        assert!(
            destinations.iter().any(|channel| channel.id == destination),
            "authoritative channels remain selectable without a local channel descriptor"
        );
        let (tx, rx) = async_channel::unbounded();
        app.workspace_model_tx = tx;
        drop(app.move_pane_to_channel(pane.id, destination));
        assert!(matches!(rx.try_recv().unwrap(), ModelEdit::ChangeView {
            scope, expected_revision: 17, change: Change::Transfer { id, destination: Scope::Channel(to), .. }, ..
        } if scope == worktree && id == pane.id && to == destination));
        assert!(app.card_state(pane.id).unwrap().existence.is_open());
    }

    #[test]
    fn move_palette_filters_destinations_and_waits_for_host_acceptance() {
        let (mut app, source, destination, pane) = channel_move_fixture();
        app.workspace_model
            .channels
            .iter_mut()
            .find(|channel| channel.id == destination)
            .unwrap()
            .title = "Destination needle".into();
        let (tx, rx) = async_channel::unbounded();
        app.workspace_model_tx = tx;
        drop(app.update_palette(super::super::palette_ui::Action::MovePane(pane.id)));
        assert!(app.palette.is_open());
        drop(app.update_palette(super::super::palette_ui::Action::Query("needle".into())));
        drop(app.update_palette(super::super::palette_ui::Action::Submit));
        assert!(
            !app.palette.is_open(),
            "dismiss before dispatch so the move passes the modal guard"
        );
        assert_eq!(
            app.settings.palette_recents.first().map(String::as_str),
            Some("command:move_pane_to_channel"),
            "parameterized destinations share the reachable command's recent identity"
        );
        assert!(matches!(rx.try_recv().unwrap(), ModelEdit::ChangeView {
            scope: Scope::Channel(from), change: Change::Transfer { id, destination: Scope::Channel(to), .. }, ..
        } if from == source && id == pane.id && to == destination));
        assert!(
            app.durable
                .channels
                .iter()
                .find(|channel| channel.id == source)
                .unwrap()
                .cards
                .contains(&pane.id),
            "choosing a destination must not optimistically detach the live pane"
        );
        assert!(app.pane_transfer_pending(pane.id));
    }

    #[test]
    fn move_picker_enter_keeps_the_shown_channel_across_newer_model_destinations() {
        for remove_original in [false, true] {
            let (mut app, source, destination, pane) = channel_move_fixture();
            let (tx, rx) = async_channel::unbounded();
            app.workspace_model_tx = tx;
            drop(app.update_palette(super::super::palette_ui::Action::MovePane(pane.id)));
            let mut replacement = app
                .workspace_model
                .channels
                .iter()
                .find(|channel| channel.id == destination)
                .unwrap()
                .clone();
            replacement.id = ChannelId::new();
            replacement.ordinal = 99;
            if remove_original {
                app.workspace_model
                    .channels
                    .retain(|channel| channel.id != destination);
            }
            app.workspace_model.channels.push(replacement);
            app.workspace_model.sequence += 1;
            drop(app.update_palette(super::super::palette_ui::Action::Submit));
            if remove_original {
                assert!(
                    rx.try_recv().is_err(),
                    "a vanished target cannot be replaced by the new channel at the same index"
                );
                assert!(!app.pane_transfer_pending(pane.id));
                assert!(
                    app.notices
                        .recent()
                        .any(|entry| entry.notice.message.contains("Choose another channel"))
                );
            } else {
                assert!(
                    matches!(rx.try_recv().unwrap(), ModelEdit::ChangeView { scope: Scope::Channel(from), change: Change::Transfer { destination: Scope::Channel(to), .. }, .. } if from == source && to == destination),
                    "Enter retains the originally shown target even when a newer channel sorts first"
                );
            }
        }
    }

    #[test]
    fn a_queued_channel_drop_from_an_old_workspace_is_ignored() {
        let (mut app, _, destination, pane) = channel_move_fixture();
        let (tx, rx) = async_channel::unbounded();
        app.workspace_model_tx = tx;
        drop(app.update(Message::PaneDropToChannel {
            generation: app.workspace_view_generation.wrapping_add(1),
            card: pane.id,
            channel: destination,
        }));
        assert!(rx.try_recv().is_err());
        assert!(!app.pane_transfer_pending(pane.id));
    }

    #[test]
    fn batched_projection_preserves_large_workspace_history_and_pending_starts() {
        let (mut app, channel, _, pane) = fixture();
        let repository = PathBuf::from("/repo/.git");
        let paths = (0..1105)
            .map(|index| PathBuf::from(format!("/repo/worktree-{index}")))
            .collect::<Vec<_>>();
        let worktrees = paths
            .iter()
            .map(|path| {
                crate::sidebar::SidebarWorktree::new(
                    crate::model::WorktreeId::new(),
                    path.clone(),
                    None,
                    false,
                    false,
                    None,
                    None,
                    None,
                )
            })
            .collect();
        app.sidebar_model.projects.push(SidebarProject::new(
            ProjectId::new(),
            RepositoryId::new(),
            "repo".into(),
            repository.clone(),
            worktrees,
            vec![],
        ));
        app.known_worktree_roots.extend(paths.iter().cloned());
        app.active_workspace = Some(PathBuf::from("/repo"));
        let terminal = fresh_terminal_session(LaunchRecipe::shell(paths[0].clone()).unwrap());
        let durable = Arc::make_mut(&mut app.durable);
        durable.cards.clear();
        for index in 0..1024 {
            let id = CardId::new();
            let existence = if index < 172 {
                Existence::Open
            } else {
                Existence::Closed {
                    tombstone: crate::model::Tombstone {
                        operation_id: None,
                        closed_at_unix_ms: 1,
                        reason: Some("historical pane".into()),
                    },
                }
            };
            durable.cards.push(CardState {
                id,
                generation: durable.generation,
                title: format!("pane {index}"),
                width_fraction: pane.width_fraction,
                existence,
                exposure: Exposure::Offscreen,
                residency: Residency::Cold,
                kind: if index % 3 == 0 {
                    CardKind::Terminal(fresh_terminal_session(
                        LaunchRecipe::shell(paths[index].join("src")).unwrap(),
                    ))
                } else {
                    pane.kind.clone()
                },
            });
            if index % 3 != 0 {
                durable.card_scopes.insert(id, paths[index].clone());
            }
            if index % 7 == 0 {
                durable.channels[0].cards.push(id);
            }
        }
        let pending_id = durable.cards[0].id;
        // A channel takes precedence over an explicit worktree scope; the host
        // pending-start bit is carried even without a local launch intention.
        let mut pending = pane.clone();
        pending.id = pending_id;
        pending.kind = CardKind::Terminal(terminal);
        pending.pending_terminal_start = true;
        let mut view = View::empty(channel);
        view.panes.push(pending);
        app.workspace_model.views.push(view);
        let retained = app.durable.cards[1].id;
        app.shared_views.retained.insert(retained);
        app.chrome_ui
            .user_titles
            .insert(app.durable.cards[2].id, "custom title".into());

        let started = Instant::now();
        let batched = app.local_views();
        let batched_elapsed = started.elapsed();
        let started = Instant::now();
        let scopes = app.local_view_scopes();
        let reference = scopes
            .iter()
            .map(|scope| (scope.clone(), app.local_view_panes(scope)))
            .collect::<HashMap<_, _>>();
        let reference_elapsed = started.elapsed();
        assert_eq!(batched.scopes, scopes);
        assert_eq!(batched.panes, reference);
        assert!(batched.panes[&Scope::Channel(channel)][0].pending_terminal_start);
        assert!(
            batched.panes[&Scope::Worktree {
                repository,
                path: paths[1023].clone(),
            }]
                .is_empty()
        );
        eprintln!(
            "large workspace projection: indexed {batched_elapsed:?}, reference {reference_elapsed:?}"
        );
    }
    #[test]
    fn shared_layout_echo_preserves_observed_terminal_exit() {
        let (mut app, channel, _, mut pane) = fixture();
        pane.kind = CardKind::Terminal(fresh_terminal_session(
            LaunchRecipe::shell(PathBuf::from("/repo")).unwrap(),
        ));
        let mut view = View::empty(channel);
        view.panes.push(pane.clone());
        app.workspace_model.views = vec![view.clone()];
        let owned = HashSet::from([pane.id]);
        drop(app.project_shared_view(&view, &owned));
        app.cards.insert(pane.id, CardRuntime::Completed);
        app.retire_ended_terminal(pane.id, 17, None, false);
        let observed = ended_terminal(app.card_state(pane.id))
            .unwrap()
            .continuity
            .clone();

        // The registry's pane descriptor can predate the runtime event, even
        // while its layout revision is newer. It does not revoke that event.
        drop(app.project_shared_view(&view, &owned));
        assert_eq!(
            ended_terminal(app.card_state(pane.id)).unwrap().continuity,
            observed
        );

        let CardKind::Terminal(incoming) = &mut view.panes[0].kind else {
            panic!()
        };
        incoming.transcript_path = Some(PathBuf::from("/new/transcript"));
        incoming.launch.arguments.push("changed".into());
        let expected_launch = incoming.launch.clone();
        let TerminalContinuity::RelaunchAvailable { recipe, .. } = &mut incoming.continuity else {
            panic!()
        };
        *recipe = expected_launch.clone();
        drop(app.project_shared_view(&view, &owned));
        let saved = ended_terminal(app.card_state(pane.id)).unwrap();
        assert_eq!(saved.launch, expected_launch);
        assert_eq!(
            saved.transcript_path.as_deref(),
            Some(Path::new("/new/transcript"))
        );
        let TerminalContinuity::RelaunchAvailable {
            last_exit: observed_exit,
            ..
        } = observed
        else {
            panic!()
        };
        assert_eq!(
            saved.continuity,
            TerminalContinuity::RelaunchAvailable {
                recipe: expected_launch,
                last_exit: observed_exit,
            }
        );
    }

    #[test]
    fn shared_projection_does_not_turn_hydrating_prior_exit_into_a_settled_end() {
        let (mut app, channel, _, mut pane) = fixture();
        pane.kind = CardKind::Terminal(fresh_terminal_session(
            LaunchRecipe::shell(PathBuf::from("/repo")).unwrap(),
        ));
        let mut view = View::empty(channel);
        view.panes.push(pane.clone());
        app.workspace_model.views = vec![view.clone()];
        let owned = HashSet::from([pane.id]);
        drop(app.project_shared_view(&view, &owned));
        app.retire_ended_terminal(pane.id, 17, None, false);
        app.cards.insert(pane.id, CardRuntime::Loading);
        app.card_state_mut(pane.id).unwrap().residency = Residency::Hydrating { epoch: 1 };
        assert!(!app.terminal_start_in_flight(pane.id));

        // Projection admits a waiting pane as Cold. Evidence must come from
        // before that change, while the old exit still belongs to a relaunch.
        drop(app.project_shared_view(&view, &owned));
        assert_eq!(app.card_state(pane.id).unwrap().kind, pane.kind);
        assert!(ended_terminal(app.card_state(pane.id)).is_none());
    }

    #[test]
    fn shared_exit_overlay_respects_new_identity_proof_and_explicit_end() {
        let mut app = super::super::tests::context_inspector_test_app(true);
        let id = app.durable.cards[0].id;
        app.retire_ended_terminal(id, 17, None, false);
        let card = app.card_state(id).unwrap().clone();
        let CardKind::Terminal(session) = &card.kind else {
            panic!()
        };
        let mut missing = session.clone();
        missing.continuity = TerminalContinuity::RelaunchAvailable {
            recipe: missing.launch.clone(),
            last_exit: None,
        };
        let mut new_identity = missing.clone();
        new_identity.id = TerminalId::new();
        let mut live = missing.clone();
        live.continuity = TerminalContinuity::Live {
            host_session_id: crate::model::HostSessionId::new("replacement"),
            boot_id: crate::model::BootId::new("replacement"),
            host_incarnation: crate::model::HostIncarnation::new("replacement"),
        };
        let mut explicit_end = missing.clone();
        explicit_end.continuity = TerminalContinuity::Exited {
            last_exit: terminal_exit(23, None),
        };
        for session in [new_identity, live, explicit_end] {
            let mut kind = CardKind::Terminal(session);
            let expected = kind.clone();
            preserve_observed_terminal_exit(&card, &mut kind);
            assert_eq!(kind, expected);
        }
        let mut hydrating = card;
        hydrating.residency = Residency::Hydrating { epoch: 1 };
        let mut kind = CardKind::Terminal(missing);
        let expected = kind.clone();
        preserve_observed_terminal_exit(&hydrating, &mut kind);
        assert_eq!(
            kind, expected,
            "an in-flight relaunch is not a settled exit"
        );
    }

    #[test]
    fn shared_terminal_reconcile_does_not_block_native_input_on_autosave_contention() {
        fn run(closed_owner: bool) -> Duration {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("state.sqlite3");
            let mut app = super::super::tests::context_inspector_test_app(true);
            let mut card = app.durable.cards[0].clone();
            let source_id = card.id;
            if closed_owner {
                card.id = CardId::new();
            }
            assert!(matches!(card.kind, CardKind::Terminal(_)));
            let channel = ChannelId::new();
            Arc::make_mut(&mut app.durable)
                .channels
                .push(ProjectChannel {
                    id: channel,
                    project_id: ProjectId::new(),
                    title: "shared terminal".into(),
                    title_source: Default::default(),
                    ordinal: 0,
                    shared: true,
                    cards: vec![source_id],
                    focused: Some(source_id),
                });
            let mut view = View::empty(Scope::Channel(channel));
            view.panes.push(Pane::from_card(&card, None).unwrap());
            app.workspace_model.views.push(view);
            app.shared_views.initialized.insert(Scope::Channel(channel));
            let (runtime, _) = crate::session::SessionRuntime::open(
                &path,
                crate::model::BootId::new("adoption-contention"),
            )
            .unwrap();
            runtime.snapshot(Arc::clone(&app.durable));
            runtime.flush(Duration::from_secs(5)).unwrap();
            let store = runtime.operation_store();
            app.session = Some(runtime);
            if closed_owner {
                let durable = Arc::make_mut(&mut app.durable);
                durable.cards[0].existence = Existence::Closed {
                    tombstone: crate::model::Tombstone {
                        operation_id: None,
                        closed_at_unix_ms: 123,
                        reason: Some("already closed historical owner".into()),
                    },
                };
                durable.focused = None;
                let navigation = durable
                    .channels
                    .iter_mut()
                    .find(|item| item.id == channel)
                    .unwrap();
                navigation.cards.clear();
                navigation.focused = None;
                app.session
                    .as_ref()
                    .unwrap()
                    .snapshot(Arc::clone(&app.durable));
                app.session
                    .as_ref()
                    .unwrap()
                    .flush(Duration::from_secs(5))
                    .unwrap();
                let CardKind::Terminal(terminal) = &card.kind else {
                    unreachable!()
                };
                assert!(
                    store
                        .closed_terminal_pane_adoption(&app.durable, card.id, terminal.id)
                        .unwrap()
                        .is_some(),
                    "fixture needs an actual closed DB recipe owner"
                );
            }
            let (entered, ready) = std::sync::mpsc::channel();
            let (begin_delay, begin) = std::sync::mpsc::channel();
            let blocker = std::thread::spawn(move || {
                let mut connection = rusqlite::Connection::open(path).unwrap();
                let transaction = connection
                    .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                    .unwrap();
                entered.send(()).unwrap();
                if begin.recv_timeout(Duration::from_secs(4)).is_ok() {
                    std::thread::sleep(Duration::from_millis(600));
                }
                transaction.rollback().unwrap();
            });
            ready.recv_timeout(Duration::from_secs(2)).unwrap();
            let mut pending = (*app.durable).clone();
            pending.cards[0].title = "ordinary pending autosave".into();
            app.session.as_ref().unwrap().snapshot(Arc::new(pending));
            let deadline = Instant::now() + Duration::from_secs(2);
            while !store.audit_writer_holds_connection() {
                assert!(
                    Instant::now() < deadline,
                    "autosave did not acquire its connection"
                );
                std::thread::sleep(Duration::from_millis(1));
            }
            begin_delay.send(()).unwrap();
            // This is the actual synchronous native model-update callback.
            let started = Instant::now();
            drop(app.reconcile_shared_views());
            let elapsed = started.elapsed();
            blocker.join().unwrap();
            app.session
                .as_ref()
                .unwrap()
                .flush(Duration::from_secs(5))
                .unwrap();
            eprintln!(
                "shared terminal reconciliation closed_owner={closed_owner} blocked native input for {elapsed:?}"
            );
            if closed_owner {
                let restored = store
                    .load_card(card.id, None)
                    .unwrap()
                    .expect("canonical pane checkpoint lands");
                assert!(restored.existence.is_open());
                assert!(
                    !store
                        .load_card(source_id, None)
                        .unwrap()
                        .unwrap()
                        .existence
                        .is_open()
                );
            }
            elapsed
        }
        let normal = run(false);
        let restoration = run(true);
        assert!(
            normal < Duration::from_millis(100) && restoration < Duration::from_millis(100),
            "native model callbacks waited on contended autosave: normal={normal:?}, closed-owner restoration={restoration:?}"
        );
    }

    #[test]
    fn closed_terminal_shared_adoption_preserves_closure_and_both_drafts() {
        let directory = tempfile::tempdir().unwrap();
        let mut app = super::super::tests::context_inspector_test_app(true);
        let mut canonical = app.durable.cards[0].clone();
        let alias = canonical.id;
        canonical.id = CardId::new();
        let pane = Pane::from_card(&canonical, None).unwrap();
        let durable = Arc::make_mut(&mut app.durable);
        durable.cards[0].existence = Existence::Closed {
            tombstone: crate::model::Tombstone {
                operation_id: None,
                closed_at_unix_ms: 123,
                reason: Some("closed by user".into()),
            },
        };
        durable.focused = None;
        durable.chat_drafts.insert(alias, "historical draft".into());
        durable
            .chat_drafts
            .insert(canonical.id, "canonical draft".into());
        let before = durable.clone();
        let (runtime, _) = crate::session::SessionRuntime::open(
            directory.path().join("state.sqlite3"),
            crate::model::BootId::new("closed-owner"),
        )
        .unwrap();
        runtime.snapshot(Arc::clone(&app.durable));
        runtime.flush(Duration::from_secs(5)).unwrap();
        let store = runtime.operation_store();
        app.session = Some(runtime);
        app.prepare_terminal_restoration(&pane);
        app.prepare_terminal_restoration(&pane);
        assert_eq!(app.shared_views.restorations.len(), 1);
        assert!(app.shared_views.adoptions.is_empty());
        assert_eq!(*app.durable, before);
        assert!(app.terminal_starts.lock().unwrap().is_empty());
        Arc::make_mut(&mut app.durable)
            .cards
            .push(canonical.clone());
        app.session.as_ref().unwrap().snapshot_terminal_panes(
            Arc::clone(&app.durable),
            app.shared_views.adoptions.clone(),
            app.shared_views.restorations.clone(),
        );
        app.session
            .as_ref()
            .unwrap()
            .flush(Duration::from_secs(5))
            .unwrap();
        assert_eq!(
            store.load_card(alias, None).unwrap().unwrap().existence,
            before.cards[0].existence
        );
        assert!(
            store
                .load_card(canonical.id, None)
                .unwrap()
                .unwrap()
                .existence
                .is_open()
        );
        assert_eq!(app.durable.chat_drafts, before.chat_drafts);
        app.shared_views.restorations.clear();
        app.prepare_terminal_restoration(&pane);
        assert!(app.shared_views.adoptions.is_empty());
        // Repeated shared-view refreshes retain coalesced autosave behavior.
        // The ordered writer already owns the first restoration checkpoint.
        assert!(app.shared_views.restorations.is_empty());
        let canonical_card = Arc::make_mut(&mut app.durable)
            .cards
            .iter_mut()
            .find(|card| card.id == canonical.id)
            .unwrap();
        canonical_card.existence = before.cards[0].existence.clone();
        // A close observed by persistence clears the memo before any later
        // projection can reopen the same canonical identity.
        app.shared_views.prune_restoration_requests(&app.durable);
        assert!(app.shared_views.restoration_requests.is_empty());
        app.prepare_terminal_restoration(&pane);
        assert_eq!(app.shared_views.restorations.len(), 1);
    }

    #[test]
    fn shared_pull_request_adoption_initializes_native_viewer() {
        let (mut app, channel, _, mut pane) = fixture();
        pane.kind = CardKind::PullRequests(crate::model::PullRequestView {
            repository: PathBuf::from("/repo/.git"),
            selected_number: None,
            file_panel_width: None,
            file_panel_collapsed: None,
        });
        let mut view = View::empty(channel);
        view.panes.push(pane.clone());
        drop(app.project_shared_view(&view, &HashSet::from([pane.id])));
        assert!(matches!(
            app.cards.get(&pane.id),
            Some(CardRuntime::PullRequests(_))
        ));
        assert_eq!(app.card_state(pane.id).unwrap().residency, Residency::Hot);
    }

    #[test]
    fn remote_shared_native_panes_do_not_initialize_local_viewers() {
        // Device selection is process-wide and cannot be reset. Test the real
        // remote guards without changing the backend of parallel local tests.
        const CHILD: &str = "CAGENTS_TEST_REMOTE_NATIVE_PANES";
        if std::env::var_os(CHILD).is_none() {
            let config = tempfile::tempdir().unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    concat!(
                        "app::shared_views::tests::",
                        "remote_shared_native_panes_do_not_initialize_local_viewers"
                    ),
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .env("XDG_CONFIG_HOME", config.path())
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
            return;
        }
        let config = crate::device::configuration_path().unwrap();
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        std::fs::write(
            &config,
            serde_json::to_vec(&serde_json::json!({
                "devices": [{"name": "test-remote", "host": "localhost", "executable": "cagents"}]
            }))
            .unwrap(),
        )
        .unwrap();
        crate::device::select(Some("test-remote")).unwrap();
        let (mut app, channel, _, mut pane) = fixture();
        let repository = PathBuf::from("/remote-only/project");
        for kind in [
            CardKind::Repository(RepositoryView {
                project: ProjectId::new(),
                repository: repository.clone(),
                worktree: repository.clone(),
                selected_file: Some("file.rs".into()),
            }),
            CardKind::PullRequests(crate::model::PullRequestView {
                repository,
                selected_number: Some(1),
                file_panel_width: None,
                file_panel_collapsed: None,
            }),
        ] {
            pane.kind = kind.clone();
            let mut view = View::empty(channel);
            view.panes.push(pane.clone());
            drop(app.project_shared_view(&view, &HashSet::from([pane.id])));
            assert!(
                matches!(app.cards.get(&pane.id), Some(CardRuntime::Failed(reason))
                if reason.contains("unavailable on remote devices"))
            );
            assert_eq!(app.card_state(pane.id).unwrap().kind, kind);
        }
    }

    #[test]
    fn shared_repository_adoption_and_retarget_initialize_native_viewers() {
        let (mut app, channel, _, mut pane) = fixture();
        let root = tempfile::tempdir().unwrap();
        let repository = RepositoryView {
            project: ProjectId::from_uuid(uuid::Uuid::nil()),
            repository: root.path().to_path_buf(),
            worktree: root.path().to_path_buf(),
            selected_file: Some(PathBuf::from("first.rs")),
        };
        pane.kind = CardKind::Repository(repository.clone());
        let mut view = View::empty(channel);
        view.panes.push(pane.clone());
        drop(app.project_shared_view(&view, &HashSet::from([pane.id])));
        let Some(CardRuntime::Repository(runtime)) = app.cards.get(&pane.id) else {
            panic!("a shared repository must start its native viewer immediately");
        };
        assert_eq!(runtime.selected_path(), Some(Path::new("first.rs")));
        assert_eq!(app.card_state(pane.id).unwrap().residency, Residency::Hot);

        let mut replacement = repository;
        replacement.selected_file = Some(PathBuf::from("second.rs"));
        view.panes[0].kind = CardKind::Repository(replacement.clone());
        drop(app.project_shared_view(&view, &HashSet::from([pane.id])));
        let Some(CardRuntime::Repository(runtime)) = app.cards.get(&pane.id) else {
            panic!("a replacement shared descriptor must start a new viewer");
        };
        assert_eq!(runtime.descriptor, replacement);
        assert_eq!(runtime.selected_path(), Some(Path::new("second.rs")));
    }

    #[test]
    fn checkout_descriptors_keep_local_project_and_live_repository_selection() {
        let (mut app, channel, _, mut pane) = fixture();
        let root = tempfile::tempdir().unwrap();
        let project = ProjectId::new();
        app.sidebar_model.projects.push(SidebarProject::new(
            project,
            RepositoryId::new(),
            "repo".into(),
            root.path().join(".git"),
            vec![crate::sidebar::SidebarWorktree::new(
                crate::model::WorktreeId::new(),
                root.path().to_path_buf(),
                Some("main".into()),
                false,
                false,
                None,
                None,
                None,
            )],
            vec![],
        ));
        let descriptor = RepositoryView {
            project,
            repository: root.path().to_path_buf(),
            worktree: root.path().to_path_buf(),
            selected_file: None,
        };
        pane.kind = CardKind::Repository(descriptor.clone());
        let mut view = View::empty(channel);
        view.panes.push(pane.clone());
        drop(app.project_shared_view(&view, &HashSet::from([pane.id])));
        let mut runtime = RepositoryRuntime::file_list_fixture(descriptor.clone(), &["first.rs"]);
        drop(runtime.update(RepositoryAction::OpenFile {
            card: pane.id,
            path: "first.rs".into(),
        }));
        app.cards
            .insert(pane.id, CardRuntime::Repository(Box::new(runtime)));
        view.panes[0].kind = views::shared_kind(&pane.kind);
        drop(app.project_shared_view(&view, &HashSet::from([pane.id])));
        let Some(CardRuntime::Repository(runtime)) = app.cards.get(&pane.id) else {
            panic!("the local repository runtime must remain attached");
        };
        assert_eq!(runtime.descriptor.project, project);
        assert_eq!(
            runtime.selected_path(),
            Some(Path::new("first.rs")),
            "remapping a window-local project identity must not recreate its live viewer"
        );
    }

    #[test]
    fn retargeted_shared_repository_preserves_unsaved_document() {
        let (mut app, channel, _, mut pane) = fixture();
        let runtime = RepositoryRuntime::dirty_fixture(pane.id);
        let original = runtime.descriptor.clone();
        pane.kind = CardKind::Repository(original.clone());
        let mut view = View::empty(channel);
        view.panes.push(pane.clone());
        drop(app.project_shared_view(&view, &HashSet::from([pane.id])));
        app.cards
            .insert(pane.id, CardRuntime::Repository(Box::new(runtime)));
        let mut replacement = original.clone();
        replacement.selected_file = Some(PathBuf::from("other.rs"));
        view.panes[0].kind = CardKind::Repository(replacement);
        drop(app.project_shared_view(&view, &HashSet::from([pane.id])));
        let Some(CardRuntime::Repository(runtime)) = app.cards.get(&pane.id) else {
            panic!("shared metadata must not discard an unsaved document");
        };
        assert!(runtime.is_dirty());
        assert_eq!(runtime.descriptor, original);
    }

    #[cfg(not(feature = "integrated-browser"))]
    #[test]
    fn browserless_shared_browser_keeps_saved_descriptor_and_external_open_state() {
        let (mut app, channel, _, pane) = fixture();
        let mut view = View::empty(channel);
        view.panes.push(pane.clone());
        drop(app.project_shared_view(&view, &HashSet::from([pane.id])));
        assert_eq!(app.card_state(pane.id).unwrap().kind, pane.kind);
        assert!(
            matches!(app.cards.get(&pane.id), Some(CardRuntime::Failed(reason))
            if reason.contains("Open the saved URL in your system browser"))
        );
    }

    #[test]
    fn two_clients_project_identical_pane_ids_and_peer_removal() {
        let (mut first, a, _, pane) = fixture();
        let (mut second, _, _, _) = fixture();
        Arc::make_mut(&mut second.durable).channels = first.durable.channels.clone();
        second.shared_views.initialized.insert(Scope::Channel(a));
        let mut view = View::empty(a);
        view.panes.push(pane.clone());
        for app in [&mut first, &mut second] {
            app.workspace_model.views = vec![view.clone()];
            drop(app.reconcile_shared_views());
            assert_eq!(
                app.durable
                    .channels
                    .iter()
                    .find(|c| c.id == a)
                    .unwrap()
                    .cards,
                vec![pane.id]
            );
            assert_eq!(app.card_state(pane.id).unwrap().kind, pane.kind);
        }
        for app in [&mut first, &mut second] {
            app.workspace_model.views[0].panes.clear();
            drop(app.reconcile_shared_views());
            assert!(!app.card_state(pane.id).unwrap().existence.is_open());
        }
    }

    /// A peer closing the pane this window has focused hands the strip's focus
    /// to a neighbour, and nothing else about the strip changes, so no
    /// viewport update carries the handoff into the saved state. Storage
    /// refuses a workspace that focuses a closed pane, and a refused workspace
    /// fails every save barrier until a later one lands.
    #[test]
    fn a_peer_closing_the_focused_pane_leaves_a_focus_storage_accepts() {
        let (mut app, a, _, first) = fixture();
        app.active_workspace = Some(PathBuf::from("/repo"));
        Arc::make_mut(&mut app.durable).active_channel = Some(a);
        let mut second = first.clone();
        second.id = CardId::new();
        if let CardKind::Browser(browser) = &mut second.kind {
            browser.id = BrowserId::new();
        }
        let mut view = View::empty(a);
        view.panes = vec![first.clone(), second.clone()];
        app.workspace_model.views = vec![view.clone()];
        drop(app.reconcile_shared_views());
        let update = app.workspace.focus(second.id).unwrap();
        drop(app.apply_viewport(update, false, false));
        assert_eq!(app.durable.focused, Some(second.id));
        crate::persistence::validate_workspace(&app.persistent_workspace()).unwrap();

        view.panes = vec![first.clone()];
        app.workspace_model.views = vec![view];
        drop(app.reconcile_shared_views());
        assert!(!app.card_state(second.id).unwrap().existence.is_open());
        assert_eq!(app.workspace.focused(), Some(first.id));
        crate::persistence::validate_workspace(&app.persistent_workspace()).unwrap();
        assert_eq!(
            app.durable.focused,
            Some(first.id),
            "the saved focus follows the strip to the pane that took it"
        );
    }

    #[test]
    fn transfer_preserves_runtime_when_destination_is_projected_first() {
        let (mut app, a, b, pane) = fixture();
        Arc::make_mut(&mut app.durable).active_channel = Some(a);
        let owned = HashSet::from([pane.id]);
        let mut source = View::empty(a);
        source.panes.push(pane.clone());
        app.shared_views.applying = true;
        drop(app.project_shared_view(&source, &owned));
        app.shared_views.applying = false;
        let mut target = View::empty(b);
        target.panes.push(pane.clone());
        app.workspace_model.views = vec![target, View::empty(a)];
        drop(app.reconcile_shared_views());
        assert!(app.card_state(pane.id).unwrap().existence.is_open());
        assert!(app.cards.contains_key(&pane.id));
        assert!(
            app.workspace.card(pane.id).is_none(),
            "transferred pane is absent from active source strip"
        );
        assert_eq!(
            app.durable
                .channels
                .iter()
                .find(|c| c.id == b)
                .unwrap()
                .cards,
            vec![pane.id]
        );
        for scope in [Scope::Channel(a), Scope::Channel(b)] {
            assert_eq!(
                app.shared_views.baseline.get(&scope),
                Some(&app.local_view_panes(&scope)),
                "the baseline describes the completed transfer"
            );
        }
    }
    #[test]
    fn peer_channel_deletion_retains_local_chat_work_in_worktree() {
        let (mut app, a, _, _) = fixture();
        let path = PathBuf::from("/repo");
        app.active_workspace = Some(path.clone());
        app.known_worktree_roots.insert(path.clone());
        Arc::make_mut(&mut app.durable).active_channel = Some(a);
        drop(app.new_native_chat(crate::agent::Provider::Codex, Some(path.join("nested"))));
        let card = app.workspace.focused().unwrap();
        let target = chat_ui::Target {
            card,
            generation: 1,
        };
        drop(app.update(Message::ChatUi(chat_ui::Action::Composer(
            target,
            cosmic::widget::text_editor::Action::Edit(cosmic::widget::text_editor::Edit::Paste(
                Arc::new("unsent peer draft".into()),
            )),
        ))));
        drop(app.retire_peer_channel(a));
        assert!(app.card_state(card).unwrap().existence.is_open());
        assert!(app.cards.contains_key(&card));
        assert!(
            !app.durable
                .channels
                .iter()
                .any(|channel| channel.cards.contains(&card))
        );
        assert_eq!(app.durable.card_scopes.get(&card), Some(&path));
        assert_eq!(
            app.persistent_workspace()
                .chat_drafts
                .get(&card)
                .map(String::as_str),
            Some("unsent peer draft")
        );
    }

    #[test]
    fn empty_shared_views_keep_unused_defaults_local_until_used() {
        let (mut app, a, _, _) = fixture();
        app.active_workspace = Some(PathBuf::from("/repo"));
        Arc::make_mut(&mut app.durable).active_channel = Some(a);
        drop(app.add_default_pane(PathBuf::from("/repo"), settings::DefaultPane::NativeChat));
        let card = app.workspace.focused().unwrap();
        assert!(app.chat_is_unused_default(card));
        assert!(app.local_view_panes(&Scope::Channel(a)).is_empty());
        drop(app.project_shared_view(&View::empty(a), &HashSet::new()));
        assert!(app.workspace.card(card).is_some());
        assert!(app.card_state(card).unwrap().existence.is_open());
        app.chat_ui = chat_ui::State::with_drafts(HashMap::from([(card, "draft".into())]));
        assert_eq!(app.local_view_panes(&Scope::Channel(a))[0].id, card);
    }
    #[test]
    fn worktree_projection_keeps_browser_ownership_and_durable_order() {
        let (mut app, _, _, first) = fixture();
        let path = PathBuf::from("/repo");
        app.sidebar_model.projects.push(SidebarProject::new(
            ProjectId::new(),
            RepositoryId::new(),
            "repo".into(),
            path.clone(),
            vec![],
            vec![],
        ));
        app.active_workspace = Some(path.clone());
        app.known_worktree_roots.insert(path.clone());
        let scope = Scope::Worktree {
            repository: path.clone(),
            path: path.clone(),
        };
        app.shared_views.initialized.insert(scope.clone());
        let mut second = first.clone();
        second.id = CardId::new();
        if let CardKind::Browser(browser) = &mut second.kind {
            browser.id = BrowserId::new();
        }
        let mut view = View::empty(scope.clone());
        view.panes = vec![first.clone(), second.clone()];
        app.workspace_model.views = vec![view.clone()];
        drop(app.reconcile_shared_views());
        assert_eq!(
            app.local_view_panes(&scope)
                .iter()
                .map(|pane| pane.id)
                .collect::<Vec<_>>(),
            vec![first.id, second.id]
        );
        view.panes.reverse();
        app.workspace_model.views = vec![view];
        drop(app.reconcile_shared_views());
        assert_eq!(
            app.local_view_panes(&scope)
                .iter()
                .map(|pane| pane.id)
                .collect::<Vec<_>>(),
            vec![second.id, first.id]
        );
        drop(app.select_worktree(path.clone(), false));
        assert_eq!(
            app.workspace.order().collect::<Vec<_>>(),
            vec![second.id, first.id]
        );
        assert_eq!(
            app.card_worktree(app.card_state(first.id).unwrap()),
            Some(path.as_path())
        );
        app.workspace_model.views[0].panes.clear();
        drop(app.reconcile_shared_views());
        assert!(!app.card_state(first.id).unwrap().existence.is_open());
    }

    #[test]
    fn terminal_inventory_alias_is_adopted_before_scope_removal_and_keeps_draft() {
        let (mut app, a, _, _) = fixture();
        app.active_workspace = Some(PathBuf::from("/repo"));
        Arc::make_mut(&mut app.durable).active_channel = Some(a);
        let session = fresh_terminal_session(LaunchRecipe::shell(PathBuf::from("/repo")).unwrap());
        drop(app.insert_terminal_card(session.clone(), None));
        let alias = app.workspace.focused().unwrap();
        app.chat_ui =
            chat_ui::State::with_drafts(HashMap::from([(alias, "saved local draft".into())]));
        let mut canonical = Pane::from_card(app.card_state(alias).unwrap(), None).unwrap();
        canonical.id = CardId::new();
        app.shared_views.pending.clear();
        let mut view = View::empty(a);
        view.panes.push(canonical.clone());
        app.workspace_model.views = vec![view];
        drop(app.reconcile_shared_views());
        assert!(!app.card_state(alias).unwrap().existence.is_open());
        assert!(app.card_state(canonical.id).unwrap().existence.is_open());
        assert_eq!(
            app.shared_views.adoptions,
            vec![crate::persistence::TerminalPaneAdoption {
                alias,
                canonical: canonical.id,
                terminal: session.id
            }]
        );
        assert_eq!(
            app.persistent_workspace()
                .chat_drafts
                .get(&canonical.id)
                .map(String::as_str),
            Some("saved local draft")
        );
    }

    #[test]
    fn blocked_view_does_not_partially_retire_terminal_aliases() {
        let (mut app, a, _, _) = fixture();
        app.active_workspace = Some(PathBuf::from("/repo"));
        Arc::make_mut(&mut app.durable).active_channel = Some(a);
        let mut panes = Vec::new();
        let mut aliases = Vec::new();
        for _ in 0..2 {
            let session =
                fresh_terminal_session(LaunchRecipe::shell(PathBuf::from("/repo")).unwrap());
            drop(app.insert_terminal_card(session, None));
            let alias = app.workspace.focused().unwrap();
            let mut canonical = Pane::from_card(app.card_state(alias).unwrap(), None).unwrap();
            canonical.id = CardId::new();
            aliases.push(alias);
            panes.push(canonical);
        }
        // A draft already held by the canonical identity prevents adoption of
        // the second alias. Neither alias may retire before the view projects.
        app.chat_ui = chat_ui::State::with_drafts(HashMap::from([
            (aliases[0], "first draft".into()),
            (panes[1].id, "canonical draft".into()),
        ]));
        app.shared_views.pending.clear();
        let mut view = View::empty(a);
        view.panes = panes;
        app.workspace_model.views = vec![view];
        drop(app.reconcile_shared_views());
        assert!(app.shared_views.blocked.contains(&Scope::Channel(a)));
        assert!(app.shared_views.adoptions.is_empty());
        for alias in &aliases {
            assert!(app.card_state(*alias).unwrap().existence.is_open());
        }
        assert!(
            app.persistent_workspace()
                .chat_drafts
                .contains_key(&aliases[0])
        );
        assert!(
            !app.persistent_workspace()
                .chat_drafts
                .contains_key(&aliases[1])
        );
        assert!(app.shared_chat_has_local_work(app.workspace_model.views[0].panes[1].id));
    }

    #[test]
    fn restored_pending_terminal_waits_for_registry_then_rehydrates() {
        let (mut app, a, _, _) = fixture();
        app.active_workspace = Some(PathBuf::from("/repo"));
        Arc::make_mut(&mut app.durable).active_channel = Some(a);
        let session = fresh_terminal_session(LaunchRecipe::shell(PathBuf::from("/repo")).unwrap());
        drop(app.insert_terminal_card(session.clone(), None));
        let id = app.workspace.focused().unwrap();
        app.terminal_starts.lock().unwrap().clear();
        app.shared_views.attached.insert(id);
        app.workspace_model.identity = crate::workspace_model::NO_REGISTRY;
        drop(app.start_terminal(id, session, true, false));
        assert!(app.terminal_starts.lock().unwrap().is_empty());
        app.cards.insert(id, CardRuntime::Loading);
        app.card_state_mut(id).unwrap().residency = Residency::Hydrating { epoch: 1 };
        app.lifecycle.register(
            id,
            Exposure::Visible,
            Residency::Hydrating { epoch: 1 },
            Instant::now(),
        );
        let mut pane = Pane::from_card(app.card_state(id).unwrap(), None).unwrap();
        pane.pending_terminal_start = true;
        let mut view = View::empty(a);
        view.panes.push(pane);
        app.shared_views.pending.clear();
        app.workspace_model.identity = uuid::Uuid::new_v4();
        app.workspace_model.views = vec![view];
        drop(app.reconcile_shared_views());
        assert!(app.terminal_starts.lock().unwrap().contains_key(&id));
    }

    #[test]
    fn explicit_terminal_creation_is_durable_and_waits_for_reservation() {
        let (mut app, a, _, first) = fixture();
        app.active_workspace = Some(PathBuf::from("/repo"));
        Arc::make_mut(&mut app.durable).active_channel = Some(a);
        let mut view = View::empty(a);
        view.panes.push(first.clone());
        app.workspace_model.views = vec![view.clone()];
        drop(app.project_shared_view(&view, &HashSet::from([first.id])));
        app.shared_views
            .baseline
            .insert(Scope::Channel(a), vec![first.clone()]);
        app.shared_views.pending.clear();
        let (tx, rx) = async_channel::unbounded();
        app.workspace_model_tx = tx;
        let session = fresh_terminal_session(LaunchRecipe::shell(PathBuf::from("/repo")).unwrap());
        drop(app.insert_terminal_card(session, Some((0, 0))));
        let id = app.workspace.focused().unwrap();
        assert!(app.durable.pending_terminal_creations.contains(&id));
        assert!(app.terminal_starts.lock().unwrap().is_empty());
        assert!(std::iter::from_fn(||rx.try_recv().ok()).any(|edit| matches!(edit,
            ModelEdit::ChangeView { operation, change: Change::Insert { pane, before }, .. }
                if operation == id.as_uuid() && pane.id == id && pane.pending_terminal_start && before == Some(first.id))));
        let saved = app.persistent_workspace();
        assert!(saved.pending_terminal_creations.contains(&id));
        assert!(
            saved
                .cards
                .iter()
                .any(|card| card.id == id && card.existence.is_open())
        );
        // Restart before acknowledgment: the persisted intent still exports
        // the same pane and exact actor identity, and cannot spawn generically.
        app.shared_views = State::for_workspace(&saved);
        app.durable = saved;
        app.workspace_model.identity = crate::workspace_model::NO_REGISTRY;
        let pane = app
            .local_view_panes(&Scope::Channel(a))
            .into_iter()
            .find(|pane| pane.id == id)
            .unwrap();
        assert!(pane.pending_terminal_start);
        let CardKind::Terminal(session) = pane.kind.clone() else {
            panic!()
        };
        drop(app.start_terminal(id, session, true, false));
        assert!(app.terminal_starts.lock().unwrap().is_empty());
        app.workspace_model.identity = uuid::Uuid::new_v4();
        app.shared_views.initialized.insert(Scope::Channel(a));
        let mut view = View::empty(a);
        view.panes = vec![pane, first];
        app.workspace_model.views = vec![view];
        drop(app.reconcile_shared_views());
        assert!(!app.durable.pending_terminal_creations.contains(&id));
        assert!(app.terminal_starts.lock().unwrap().contains_key(&id));
    }

    #[test]
    fn automatic_terminal_waits_for_authoritative_empty_view_reservation() {
        let (mut app, a, _, _) = fixture();
        app.active_workspace = Some(PathBuf::from("/repo"));
        Arc::make_mut(&mut app.durable).active_channel = Some(a);
        let (tx, rx) = async_channel::unbounded();
        app.workspace_model_tx = tx;
        drop(app.add_default_pane(PathBuf::from("/repo"), settings::DefaultPane::Terminal));
        let id = app.workspace.focused().unwrap();
        assert!(app.shared_views.defaults.contains_key(&id));
        assert!(app.terminal_starts.lock().unwrap().is_empty());
        assert!(std::iter::from_fn(||rx.try_recv().ok()).any(|edit|matches!(edit,ModelEdit::ChangeView {change:Change::InsertIfEmpty {pane},..} if pane.id==id)));
        let mut view = View::empty(a);
        view.panes
            .push(Pane::from_card(app.card_state(id).unwrap(), None).unwrap());
        app.workspace_model.views = vec![view];
        drop(app.reconcile_shared_views());
        assert!(!app.shared_views.defaults.contains_key(&id));
        assert!(app.terminal_starts.lock().unwrap().contains_key(&id));
    }
    #[test]
    fn edit_during_seed_is_published_before_projection() {
        let (mut app, a, _, pane) = fixture();
        app.shared_views.initialized.remove(&Scope::Channel(a));
        app.shared_views.seeding.insert(Scope::Channel(a));
        app.shared_views.baseline.insert(Scope::Channel(a), vec![]);
        let (tx, rx) = async_channel::unbounded();
        app.workspace_model_tx = tx;
        drop(app.project_shared_view(
            &View {
                scope: Scope::Channel(a),
                revision: 0,
                panes: vec![pane.clone()],
                retired: vec![],
            },
            &HashSet::from([pane.id]),
        ));
        app.acknowledge_view_edit(Scope::Channel(a), None);
        assert!(std::iter::from_fn(||rx.try_recv().ok()).any(|edit|matches!(edit, ModelEdit::ChangeView {change:Change::Insert {pane: inserted,..},..} if inserted.id==pane.id)));
    }
}
