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

/// Each open card's worktree during one projection, with how many cards
/// each path holds, so a view no card resolves to is known without a scan.
#[derive(Default)]
struct CardWorktrees {
    paths: HashMap<CardId, PathBuf>,
    counts: HashMap<PathBuf, usize>,
}

impl CardWorktrees {
    fn get(&self, card: CardId) -> Option<&PathBuf> {
        self.paths.get(&card)
    }

    fn holds(&self, path: &Path) -> bool {
        self.counts.contains_key(path)
    }

    fn insert(&mut self, card: CardId, path: PathBuf) {
        self.remove(card);
        *self.counts.entry(path.clone()).or_default() += 1;
        self.paths.insert(card, path);
    }

    fn remove(&mut self, card: CardId) {
        let Some(path) = self.paths.remove(&card) else {
            return;
        };
        if let Some(count) = self.counts.get_mut(&path) {
            *count -= 1;
            if *count == 0 {
                self.counts.remove(&path);
            }
        }
    }
}

struct ScopeDiscovery<'a> {
    scopes: Vec<Scope>,
    worktree_cards: Vec<(&'a CardState, &'a Path)>,
}

/// Whole descriptors can contain concurrently changed metadata even for the
/// same process. Retry only while the complete base is still authoritative;
/// the daemon must also enforce this fence when it applies the operation.
fn can_retry_source_fields(before: &Pane, current: &Pane) -> bool {
    before.kind == current.kind
        && before.pending_terminal_start == current.pending_terminal_start
        && before.scope == current.scope
        && match &before.kind {
            CardKind::Terminal(session) => session
                .continuity
                .live_proof()
                .is_some_and(|proof| proof.is_complete()),
            CardKind::Chat(session) => session
                .proof
                .as_ref()
                .is_some_and(crate::model::HostSessionProof::is_complete),
            CardKind::Browser(_) | CardKind::Repository(_) | CardKind::PullRequests(_) => true,
            _ => false,
        }
}

pub(super) fn blocked_managed_view_scopes(
    model: &crate::workspace_model::WorkspaceModel,
    views: &[View],
) -> HashSet<Scope> {
    let mut blocked = HashSet::new();
    let display_index = views::ManagedDisplayIndex::new(model);
    let managed = model
        .agents
        .iter()
        .map(|agent| (CardId::from_uuid(agent.id.as_uuid()), agent))
        .collect::<HashMap<_, _>>();
    for view in views {
        if view.panes.iter().any(|pane| {
            managed
                .get(&pane.id)
                .is_some_and(|agent| !display_index.permits_projection(agent, pane))
        }) {
            blocked.insert(view.scope.clone());
        }
    }
    blocked
}

/// Why one of `scopes` is not admitted by the registry yet.
fn view_scope_hold<'a>(
    state: &State,
    mut scopes: impl Iterator<Item = &'a Scope>,
) -> Option<&'static str> {
    scopes.find_map(|scope| {
        if !state.initialized.contains(scope) {
            Some("the shared workspace has not admitted its view")
        } else if state.seeding.contains(scope) {
            Some("its view is still being restored into the shared workspace")
        } else if state.blocked.contains(scope) {
            Some("its view holds a managed agent this window may not show yet")
        } else {
            None
        }
    })
}

#[derive(Default)]
pub(super) struct State {
    // Full snapshots advance view/agent authority. Metadata-only folds advance
    // the outer model sequence without supplying either of these collections.
    pub(super) snapshot_sequence: u64,
    #[cfg(test)]
    local_projection_count: std::cell::Cell<usize>,
    #[cfg(test)]
    view_projection_count: std::cell::Cell<usize>,
    /// The daemon answered this window without a shared model to give.
    pub(super) registry_unavailable: bool,
    baseline: HashMap<Scope, Vec<Pane>>,
    initialized: HashSet<Scope>,
    seeding: HashSet<Scope>,
    pub(super) pending: Vec<(uuid::Uuid, Scope, Change)>,
    /// Immutable logical-source identity captured when each Update is queued.
    /// A later watcher snapshot must not redefine the origin of a stale edit.
    source_bases: HashMap<uuid::Uuid, (uuid::Uuid, PathBuf)>,
    /// Explicit moves wait for host acceptance before changing local membership.
    requested_transfers: HashMap<uuid::Uuid, CardId>,
    pub attached: HashSet<CardId>,
    retained: HashSet<CardId>,
    /// Exact chat identity retired by an authoritative view, never inferred from EOF.
    pub closed_chats: HashMap<CardId, crate::chat::Session>,
    /// Last actual canonical descriptors, before optimistic local edits.
    observed_chats: HashMap<CardId, crate::chat::Session>,
    applying: bool,
    pub creating_default: bool,
    pub deferred_default: Option<PathBuf>,
    pub defaults: HashMap<CardId, TerminalSession>,
    default_requests: HashSet<CardId>,
    pub blocked: HashSet<Scope>,
    pub adoptions: Vec<crate::persistence::TerminalPaneAdoption>,
    /// Chat cards whose shared view re-pointed them since this window last
    /// saved, which its own store must be told explicitly.
    pub chat_adoptions: HashMap<CardId, crate::session::ChatIdentityAdoption>,
    pub restorations: Vec<crate::persistence::TerminalPaneRestoration>,
    restoration_requests: HashMap<CardId, TerminalId>,
}

impl State {
    /// Treats `scope` as already seeded with `panes`, as an acknowledged seed
    /// leaves it.
    #[cfg(test)]
    pub(super) fn initialize(&mut self, scope: Scope, panes: Vec<Pane>) {
        self.baseline.insert(scope.clone(), panes);
        self.initialized.insert(scope);
    }

    /// View edits published and not yet acknowledged, oldest first.
    #[cfg(test)]
    pub(super) fn pending_changes(&self) -> impl DoubleEndedIterator<Item = (&Scope, &Change)> {
        self.pending
            .iter()
            .map(|(_, scope, change)| (scope, change))
    }

    pub(super) fn has_observed_chat(&self, card: CardId) -> bool {
        self.observed_chats.contains_key(&card)
    }

    /// Whether an edit to `card`'s pane is waiting for the registry.
    pub(super) fn has_pending_update(&self, card: CardId) -> bool {
        self.pending
            .iter()
            .any(|(_, _, change)| matches!(change, Change::Update { pane, .. } if pane.id == card))
    }

    pub(super) fn requested_transfer_card(&self, operation: uuid::Uuid) -> Option<CardId> {
        self.requested_transfers.get(&operation).copied()
    }

    pub(super) fn is_requested_transfer(&self, operation: uuid::Uuid) -> bool {
        self.requested_transfers.contains_key(&operation)
    }

    /// Retire exactly the refused intent; unrelated in-flight edits still
    /// project over the authoritative view until their own acknowledgments.
    pub(super) fn refused(&mut self, operation: uuid::Uuid) {
        self.requested_transfers.remove(&operation);
        self.source_bases.remove(&operation);
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
    pub(super) fn capture_view_update_source(&mut self, edit: &ModelEdit) {
        let ModelEdit::ChangeView {
            scope,
            operation,
            change: Change::Update { .. },
            ..
        } = edit
        else {
            return;
        };
        let repository = match scope {
            Scope::Channel(channel) => self
                .workspace_model
                .channel(*channel)
                .map(|channel| channel.repository.clone()),
            Scope::Worktree { repository, .. } => Some(repository.clone()),
        };
        if let Some(repository) = repository {
            self.shared_views
                .source_bases
                .entry(*operation)
                .or_insert((self.workspace_model.identity, repository));
        }
    }

    pub(super) fn refuse_conflicting_view_update(
        &mut self,
        scope: Scope,
        operation: uuid::Uuid,
        change: Change,
        model: crate::workspace_model::WorkspaceModel,
        error: String,
    ) -> Task<Message> {
        if self.workspace_model.identity != crate::workspace_model::NO_REGISTRY
            && model.identity != self.workspace_model.identity
        {
            return Task::none();
        }
        let repository = |model: &crate::workspace_model::WorkspaceModel, scope: &Scope| match scope
        {
            Scope::Channel(channel) => model
                .channel(*channel)
                .map(|channel| channel.repository.clone()),
            Scope::Worktree { repository, .. } => Some(repository.clone()),
        };
        let captured_repository = self
            .shared_views
            .source_bases
            .get(&operation)
            .filter(|(identity, _)| *identity == self.workspace_model.identity)
            .map(|(_, repository)| repository.clone());
        self.install_workspace_model_snapshot(model);
        let Some(index) =
            self.shared_views
                .pending
                .iter()
                .position(|(id, pending_scope, pending)| {
                    *id == operation && *pending_scope == scope && *pending == change
                })
        else {
            // An old-scope receipt cannot retire an already retargeted retry.
            return self.reconcile_workspace_model();
        };
        // Another window can publish the same session update first, as when two
        // windows attached to one conversation both learn its provider session.
        // Only a session update qualifies: a refused name or width edit can be lost.
        let session_already_applied = matches!(&change, Change::Update { before, pane } if {
            let same_source = |a: &Pane, b: &Pane| {
                a.kind == b.kind
                    && a.pending_terminal_start == b.pending_terminal_start
                    && a.scope == b.scope
            };
            let mut owners = self
                .workspace_model
                .views
                .iter()
                .flat_map(|view| &view.panes)
                .filter(|current| current.id == pane.id);
            !same_source(before, pane)
                && owners.next().is_some_and(|current| same_source(current, pane))
                && owners.next().is_none()
        });
        let mut retry = None;
        if let Change::Update { before, pane } = &change
            && before.id == pane.id
            && self
                .card_state(pane.id)
                .is_some_and(|card| card.existence.is_open())
        {
            let mut owners = self
                .workspace_model
                .views
                .iter()
                .filter(|view| view.panes.iter().any(|current| current.id == pane.id));
            if let Some(owner) = owners.next()
                && owners.next().is_none()
                && !owner.retired.contains(&pane.id)
                && !self.shared_views.blocked.contains(&owner.scope)
                && captured_repository.is_some()
                && captured_repository == repository(&self.workspace_model, &scope)
                && captured_repository == repository(&self.workspace_model, &owner.scope)
            {
                let current = owner
                    .panes
                    .iter()
                    .find(|current| current.id == pane.id)
                    .unwrap();
                let mut presentation = current.clone();
                let later = &self.shared_views.pending[index + 1..];
                let superseded = |field: fn(&Pane, &Pane) -> bool| {
                    later.iter().any(|(_, _, change)| {
                        matches!(change, Change::Update { before, pane: after }
                        if after.id == pane.id && field(before, after))
                    })
                };
                if before.title != pane.title && !superseded(|a, b| a.title != b.title) {
                    presentation.title = pane.title.clone();
                }
                if before.user_title != pane.user_title
                    && !superseded(|a, b| a.user_title != b.user_title)
                {
                    presentation.user_title = pane.user_title.clone();
                }
                if before.width_fraction != pane.width_fraction
                    && !superseded(|a, b| a.width_fraction != b.width_fraction)
                {
                    presentation.width_fraction = pane.width_fraction;
                }
                if presentation != *current {
                    retry = Some((
                        uuid::Uuid::new_v4(),
                        owner.scope.clone(),
                        owner.revision,
                        Change::Update {
                            before: current.clone(),
                            pane: presentation,
                        },
                    ));
                }
            }
        }
        // Nothing was lost once any name or width it changed is on its way again
        // or already there, so there is nothing to tell anyone about. A field it
        // did not change may have moved on since, as a suggested title does.
        let already_applied = session_already_applied
            && (retry.is_some()
                || matches!(&change, Change::Update { before, pane }
                    if self
                        .workspace_model
                        .views
                        .iter()
                        .flat_map(|view| &view.panes)
                        .any(|current| current.id == pane.id
                            && (before.title == pane.title || current.title == pane.title)
                            && (before.user_title == pane.user_title
                                || current.user_title == pane.user_title)
                            && (before.width_fraction == pane.width_fraction
                                || current.width_fraction == pane.width_fraction))));
        if already_applied {
            tracing::debug!(?scope, %operation, %error, "pane session update lost to an identical one");
        } else {
            tracing::warn!(?scope, %operation, %error, "refused source-sensitive pane intent; host applied no fields");
        }
        self.shared_views.pending.remove(index);
        let source_basis = self.shared_views.source_bases.remove(&operation);
        if let Some((operation, scope, revision, change)) = retry {
            if let Some(source_basis) = source_basis {
                self.shared_views
                    .source_bases
                    .insert(operation, source_basis);
            }
            self.shared_views
                .pending
                .insert(index, (operation, scope.clone(), change.clone()));
            self.publish_model_edit(ModelEdit::ChangeView {
                scope,
                expected_revision: revision,
                operation,
                change,
            });
        }
        if !already_applied {
            self.notices.record(TransientNotice::warning(
                "The pane session update was refused because its source changed. Independent name and width edits were retried where safe."
            ));
        }
        self.reconcile_workspace_model()
    }

    /// Refusing publication does not retire an already accepted conversation.
    /// Capture the original scoped Insert before retiring its pending overlay;
    /// an unrelated callback cannot protect or republish a different pane.
    pub(super) fn refuse_shared_view_edit(&mut self, scope: &Scope, operation: uuid::Uuid) {
        let Some((_, owner, change)) = self
            .shared_views
            .pending
            .iter()
            .find(|(id, _, _)| *id == operation)
        else {
            return;
        };
        if owner != scope {
            return;
        }
        if let Change::Insert { pane, .. } = change
            && matches!(pane.kind, CardKind::Chat(_))
        {
            let card = pane.id;
            // A newer canonical owner or retirement has its own authority.
            // An old Insert refusal must not reclassify that newer source.
            if !self.workspace_model.views.iter().any(|view| {
                view.panes.iter().any(|pane| pane.id == card) || view.retired.contains(&card)
            }) && (self.retain_chat_after_creation_refusal(card, scope)
                || self.retain_draft_after_creation_refusal(card, scope))
            {
                self.retain_local_pane(card);
            }
        }
        self.shared_views.refused(operation);
    }

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

    /// A terminal start that has to wait. An automatic or explicit creation
    /// waits for the registry to accept its pane, and a restored terminal this
    /// window may share waits for the registry to say whether it does.
    pub(super) fn terminal_start_held(&self, card: CardId, explicit_retry: bool) -> bool {
        self.shared_views.defaults.contains_key(&card)
            || self.durable.pending_terminal_creations.contains(&card)
            || (self.shared_views.attached.contains(&card)
                && self.workspace_model.identity == crate::workspace_model::NO_REGISTRY
                && !explicit_retry)
    }

    /// Whether the registry shows this card in any view, so another window
    /// may host it.
    fn listed_in_registry(&self, card: CardId) -> bool {
        self.workspace_model
            .views
            .iter()
            .any(|view| view.panes.iter().any(|pane| pane.id == card))
    }

    /// Admits again the hydration of a restored terminal whose start was held
    /// for the registry. The lifecycle already counts it as hydrating, so it
    /// would otherwise never be asked to start and its pane would stay on
    /// "Starting terminal…" for as long as the window is open.
    fn readmit_held_terminal(&mut self, card: CardId, exposure: Exposure) {
        self.cards.insert(card, CardRuntime::Loading);
        self.lifecycle
            .register(card, exposure, Residency::Cold, Instant::now());
        self.set_residency(card, Residency::Cold);
    }

    /// A restored window holds every saved terminal as possibly shared until
    /// the registry answers. Projecting a view releases that view's own held
    /// terminals, but some terminals have no projection to do it: the
    /// registry lists a window-local terminal in no view, and a listed one's
    /// view can stay unprojected while its scope loads. Once the registry has
    /// answered, or the daemon has said it has none to give, every terminal
    /// still held is released here, so none waits on a projection that never
    /// comes.
    ///
    /// A terminal no view lists belongs to this window alone. It stops
    /// counting as shared and restores the way an unshared window restores
    /// it, opened or not: a saved recipe is inspected without being run,
    /// attaching only to its exact existing terminal. One that cannot restore
    /// that way only attaches, unless the registry confirmed it is not shared
    /// and it is being opened. A listed terminal is admitted to hydrate again
    /// and only attaches, since another window may host it.
    pub(super) fn release_held_terminals(&mut self) -> Task<Message> {
        if self.shared_registry_connecting() {
            return Task::none();
        }
        self.retire_startup_registry_waits();
        // An unopened terminal is not asking to start, so it can wait until
        // the view that may yet list it has been admitted.
        let views_settled =
            self.shared_views.registry_unavailable || self.shared_views_ready_for_startup();
        let held = self
            .durable
            .cards
            .iter()
            .filter_map(|card| match &card.kind {
                CardKind::Terminal(session)
                    if !terminal_ended(session)
                        && card.existence.is_open()
                        && (views_settled
                            || matches!(self.residency(card.id), Residency::Hydrating { .. }))
                        && matches!(self.cards.get(&card.id), Some(CardRuntime::Loading))
                        && self.shared_views.attached.contains(&card.id)
                        && !self.shared_views.defaults.contains_key(&card.id)
                        && !self.durable.pending_terminal_creations.contains(&card.id)
                        && !self.terminal_start_in_flight(card.id)
                        && !self.terminal_retry_pending(card.id)
                        && !self.pane_close_owned(card.id) =>
                {
                    Some((
                        card.id,
                        self.exposure(card.id),
                        matches!(self.residency(card.id), Residency::Hydrating { .. }),
                        session.clone(),
                    ))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        let confirmed = self.workspace_model.identity != crate::workspace_model::NO_REGISTRY;
        let mut tasks = Vec::new();
        let mut hydrate = false;
        for (card, exposure, opening, session) in held {
            if !self.listed_in_registry(card) {
                self.shared_views.attached.remove(&card);
                if self.prepare_saved_terminal(card, &session) {
                    self.restored_agent_attaches
                        .retain(|queued| *queued != card);
                    tasks.push(self.inspect_saved_terminal(card));
                    continue;
                }
                if !(confirmed && opening) {
                    self.shared_views.attached.insert(card);
                    if !confirmed && !self.restored_agent_attaches.contains(&card) {
                        self.restored_agent_attaches.push_back(card);
                    }
                    continue;
                }
            }
            if opening {
                self.readmit_held_terminal(card, exposure);
                hydrate = true;
            }
        }
        if hydrate {
            tasks.push(cosmic::task::message(Message::LifecycleSweep));
        }
        Task::batch(tasks)
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

    /// Whether a shared pane is still waiting to learn the registry: none is
    /// joined yet, and the daemon has not said it has none to give.
    pub(super) fn shared_registry_connecting(&self) -> bool {
        self.workspace_model.identity == crate::workspace_model::NO_REGISTRY
            && !self.shared_views.registry_unavailable
    }

    /// Whether `card` attaches as a pane of a joined registry rather than
    /// from this window's own saved descriptor.
    pub(super) fn attaches_as_shared(&self, card: CardId) -> bool {
        self.shared_views.attached.contains(&card)
            && self.workspace_model.identity != crate::workspace_model::NO_REGISTRY
    }

    /// Whether every view this window shows has been admitted by the
    /// registry, so its panes' records are the shared ones.
    pub(super) fn shared_views_ready_for_startup(&self) -> bool {
        self.shared_views_startup_hold().is_none()
    }

    /// Why a view this window shows is not admitted by the registry yet.
    pub(super) fn shared_views_startup_hold(&self) -> Option<&'static str> {
        if self.durable.workspace_model_identity.is_none() && self.shared_views.attached.is_empty()
        {
            return None;
        }
        if self.workspace_model.identity == crate::workspace_model::NO_REGISTRY {
            return Some("the shared workspace has not answered this window");
        }
        let scopes = self.local_view_scopes();
        view_scope_hold(&self.shared_views, scopes.iter())
    }

    /// A queued row may attach only after its own canonical view is admitted.
    /// The daemon still resolves this exact terminal under its current proof;
    /// this check never compares that proof with a cached session's old proof.
    pub(super) fn shared_terminal_ready_for_restored_attach(
        &self,
        card: CardId,
        source: &TerminalSession,
        views_ready: &mut Option<bool>,
    ) -> bool {
        self.shared_terminal_attach_hold(card, source, views_ready)
            .is_none()
    }

    /// Why a queued row may not attach `source` yet. `views_ready` remembers
    /// whether every view is admitted, so a caller asking about each of many
    /// rows while nothing changes discovers the views only once.
    pub(super) fn shared_terminal_attach_hold(
        &self,
        card: CardId,
        source: &TerminalSession,
        views_ready: &mut Option<bool>,
    ) -> Option<&'static str> {
        if self.shared_views.defaults.contains_key(&card)
            || self.durable.pending_terminal_creations.contains(&card)
        {
            return Some("its pane is still being created");
        }
        if !self.shared_views.attached.contains(&card) {
            return None;
        }
        if self.workspace_model.identity == crate::workspace_model::NO_REGISTRY {
            // A daemon whose registry store is unavailable never admits the
            // view, so its rows attach from their own descriptors as before.
            return (!self.shared_views.registry_unavailable)
                .then_some("the shared workspace has not answered this window");
        }
        let mut owners = self.workspace_model.views.iter().flat_map(|view| {
            view.panes
                .iter()
                .filter(move |pane| pane.id == card)
                .map(move |pane| (view, pane))
        });
        let Some((view, pane)) = owners.next() else {
            // Once every view is admitted, no view will list it: it is this
            // window's own and attaches from its descriptor.
            return (!*views_ready.get_or_insert_with(|| self.shared_views_ready_for_startup()))
                .then_some("the shared workspace does not list its pane");
        };
        if owners.next().is_some() {
            return Some("the shared workspace lists its pane in more than one view");
        }
        if view.retired.contains(&card) {
            return Some("the shared workspace closed its pane");
        }
        if let Some(hold) = view_scope_hold(&self.shared_views, std::iter::once(&view.scope)) {
            return Some(hold);
        }
        if pane.pending_terminal_start {
            return Some("the shared workspace is still starting its terminal");
        }
        if !matches!(&pane.kind, CardKind::Terminal(current) if current == source) {
            return Some("the shared workspace names a different terminal for its pane");
        }
        None
    }

    fn local_view_scopes(&self) -> Vec<Scope> {
        self.discover_view_scopes().scopes
    }

    /// Every scope this window shows, and the worktree each card outside a
    /// channel resolves to. A large workspace holds thousands of cards and
    /// historical worktrees, so each card and path is resolved by lookup
    /// rather than by comparing it with every root or project worktree.
    fn discover_view_scopes(&self) -> ScopeDiscovery<'_> {
        let roots = self.indexed_worktree_roots();
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
        ScopeDiscovery {
            scopes,
            worktree_cards,
        }
    }

    /// The local views that list `card`, as `local_view_cards` would,
    /// without resolving every view's cards to find one.
    fn local_scopes_listing(&self, card: CardId) -> Vec<Scope> {
        let ScopeDiscovery {
            scopes,
            worktree_cards,
        } = self.discover_view_scopes();
        let worktree = worktree_cards
            .iter()
            .find(|(state, _)| state.id == card)
            .filter(|(state, _)| state.existence.is_open() && !state.kind.is_utility())
            .map(|(_, path)| *path);
        scopes
            .into_iter()
            .filter(|scope| match scope {
                Scope::Channel(id) => self
                    .durable
                    .channels
                    .iter()
                    .any(|channel| channel.id == *id && channel.cards.contains(&card)),
                Scope::Worktree { path, .. } => worktree == Some(path.as_path()),
            })
            .collect()
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

    /// A reported title waiting for its deferred save is not published yet:
    /// the pane keeps the title this window last shared for it, so neither an
    /// edit nor a captured baseline carries the newer one before that save.
    fn hold_reported_title(&self, scope: &Scope, pane: &mut Pane) {
        if !self.reported_title_save.cards.contains(&pane.id) {
            return;
        }
        // A pane moving to `scope` in this same save is still in its source's
        // baseline; a move publishes no title, so hold that one too.
        let in_scope = self.shared_views.baseline.get(scope).into_iter();
        let elsewhere = self
            .shared_views
            .baseline
            .iter()
            .filter(|(other, _)| *other != scope)
            .map(|(_, panes)| panes);
        if let Some(shared) = in_scope
            .chain(elsewhere)
            .flatten()
            .find(|shared| shared.id == pane.id)
        {
            pane.title.clone_from(&shared.title);
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
                self.hold_reported_title(scope, &mut pane);
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
        self.local_views_in(None)
    }

    fn local_views_in(&self, selected: Option<&HashSet<Scope>>) -> LocalViews {
        #[cfg(test)]
        self.shared_views
            .local_projection_count
            .set(self.shared_views.local_projection_count.get() + 1);
        let cards = self
            .durable
            .cards
            .iter()
            .map(|card| (card.id, card))
            .collect::<HashMap<_, _>>();
        let ScopeDiscovery {
            mut scopes,
            worktree_cards,
        } = self.discover_view_scopes();
        if let Some(selected) = selected {
            scopes.retain(|scope| selected.contains(scope));
        }
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
                    self.hold_reported_title(scope, &mut pane);
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

    fn pane_move_lifecycle_pending(&self, card: CardId) -> bool {
        self.pane_close_owned(card)
            || self.terminal_start_in_flight(card)
            || self.chat_start_pending(card)
    }

    fn pane_transfer_source(&self, card: CardId) -> Option<(&View, &Path)> {
        if self.pane_move_lifecycle_pending(card)
            || self.focused_repository_is_saving()
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
        if self.shared_views.blocked.contains(&source.scope)
            || matches!(&source.scope, Scope::Channel(id) if self.channel_ui.is_deleting(*id) || self.channel_ui.was_deleted(*id))
        {
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

    #[cfg(test)]
    pub(super) fn mark_pane_moving(&mut self, card: CardId) {
        self.shared_views
            .requested_transfers
            .insert(uuid::Uuid::new_v4(), card);
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
        if self.pane_move_lifecycle_pending(card) {
            self.status = Some(TransientNotice::warning(
                "Wait for this pane to finish opening, closing or changing views before moving it.",
            ));
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

    pub(super) fn publish_new_managed_pane(&mut self, scope: Scope, card: CardId) {
        if !self.shared_views.initialized.contains(&scope)
            || self.shared_views.blocked.contains(&scope)
            || matches!(&scope, Scope::Channel(id) if self.channel_ui.was_deleted(*id) || self.channel_ui.is_deleting(*id))
            || self.workspace_model.views.iter().any(|view| {
                view.panes.iter().any(|pane| pane.id == card) || view.retired.contains(&card)
            })
        {
            return;
        }
        let Some(pane) = self
            .local_view_panes(&scope)
            .into_iter()
            .find(|pane| pane.id == card)
        else {
            return;
        };
        let baseline = self.shared_views.baseline.entry(scope.clone()).or_default();
        if baseline.iter().any(|existing| existing.id == card) {
            return;
        }
        baseline.push(pane.clone());
        let revision = self
            .workspace_model
            .views
            .iter()
            .find(|view| view.scope == scope)
            .map(|view| view.revision)
            .unwrap_or(0);
        let operation = uuid::Uuid::new_v4();
        let change = Change::Insert { pane, before: None };
        self.shared_views
            .pending
            .push((operation, scope.clone(), change.clone()));
        if matches!(change, Change::Insert { ref pane, .. } if matches!(pane.kind, CardKind::Chat(_)))
        {
            self.shared_views.attached.insert(card);
        }
        self.publish_model_edit(ModelEdit::ChangeView {
            scope,
            expected_revision: revision,
            operation,
            change,
        });
    }

    /// Passive retention preserves work, but cannot authorize a provider start
    /// after another client retired the pane or the registry refused creation.
    pub(super) fn shared_chat_creation_revoked(&self, card: CardId, scope: Option<&Scope>) -> bool {
        // A pane kept here after its creation was refused starts in this
        // window alone, while the scope it was created in is still shared.
        if self.shared_views.retained.contains(&card)
            && !scope.is_some_and(|scope| {
                self.workspace_model
                    .views
                    .iter()
                    .any(|view| &view.scope == scope)
            })
        {
            return true;
        }
        let mut owners = self
            .workspace_model
            .views
            .iter()
            .filter(|view| view.panes.iter().any(|pane| pane.id == card));
        if let Some(owner) = owners.next() {
            // A transfer retires the source while preserving the exact current
            // destination. Only that unique owner can override old tombstones.
            return owners.next().is_some()
                || Some(&owner.scope) != scope
                || owner.retired.contains(&card);
        }
        self.workspace_model
            .views
            .iter()
            .any(|view| view.retired.contains(&card))
    }

    /// An automatic empty composer has no shared descriptor yet. Once it has
    /// work, establish its one creation intent before an empty snapshot can
    /// mistake that unpublished source for a pane removed by another window.
    /// Already published, restored, refused and locally retained cards never
    /// gain fresh creation authority from absence in a snapshot.
    fn publish_unpublished_local_chats(&mut self) {
        let candidates = self
            .durable
            .cards
            .iter()
            .filter_map(|card| {
                if !card.existence.is_open()
                    || !matches!(card.kind, CardKind::Chat(_))
                    || self.shared_views.attached.contains(&card.id)
                    || self.shared_views.retained.contains(&card.id)
                    || self.chat_is_unused_default(card.id)
                {
                    return None;
                }
                let scope = self
                    .durable
                    .channels
                    .iter()
                    .find(|channel| channel.cards.contains(&card.id))
                    .map(|channel| Scope::Channel(channel.id))
                    .or_else(|| {
                        self.card_worktree(card)
                            .and_then(|path| self.worktree_view_scope(path))
                    })?;
                Some((scope, card.id))
            })
            .collect::<Vec<_>>();
        for (scope, card) in candidates {
            self.publish_new_managed_pane(scope, card);
        }
    }

    pub(super) fn publish_view_changes(&mut self) {
        if self.shared_views.applying
            || self.workspace_model.identity == crate::workspace_model::NO_REGISTRY
        {
            return;
        }
        self.publish_projected_view_changes(self.local_views());
    }

    fn publish_projected_view_changes(&mut self, projected: LocalViews) {
        if self.shared_views.applying
            || self.workspace_model.identity == crate::workspace_model::NO_REGISTRY
        {
            return;
        }
        let LocalViews {
            scopes,
            cards,
            mut panes,
        } = projected;
        // Publishing only queues edits, so the model's views stay put: find
        // each scope's first view by lookup rather than a scan per scope.
        let mut needed = scopes.iter().collect::<HashSet<_>>();
        let desired = panes
            .values()
            .flatten()
            .map(|pane| pane.id)
            .collect::<HashSet<_>>();
        for (scope, previous) in &self.shared_views.baseline {
            if previous.iter().any(|pane| desired.contains(&pane.id)) {
                needed.insert(scope);
            }
        }
        let mut view_indices = HashMap::new();
        for (index, view) in self.workspace_model.views.iter().enumerate() {
            if needed.contains(&view.scope) {
                view_indices.entry(view.scope.clone()).or_insert(index);
            }
        }
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
                    let expected_revision = view_indices
                        .get(&source)
                        .map(|&index| self.workspace_model.views[index].revision)
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
                    let closed: Vec<CardId> = view_indices
                        .get(&scope)
                        .map(|&index| &self.workspace_model.views[index])
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
                    tracing::debug!(
                        ?scope,
                        panes = legacy.len(),
                        closed = closed.len(),
                        "Migrating a local workspace view"
                    );
                    self.publish_model_edit(ModelEdit::SeedView {
                        scope: scope.clone(),
                        panes: legacy.clone(),
                        closed,
                    });
                    self.shared_views.attached.extend(
                        legacy
                            .iter()
                            .filter(|pane| matches!(pane.kind, CardKind::Chat(_)))
                            .map(|pane| pane.id),
                    );
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
            let revision = view_indices
                .get(&scope)
                .map(|&index| self.workspace_model.views[index].revision)
                .unwrap_or(0);
            for change in views::changes(&previous, &panes) {
                // A Remove is every window's instruction to close the pane. A
                // pane still open here has only lost its local placement.
                if let Change::Remove { id } = &change
                    && self
                        .card_state(*id)
                        .is_some_and(|card| card.existence.is_open())
                {
                    tracing::warn!(card = %id, ?scope, "not retiring a shared pane that is still open");
                    continue;
                }
                if let Change::Insert { pane, .. } = &change
                    && matches!(pane.kind, CardKind::Chat(_))
                {
                    self.shared_views.attached.insert(pane.id);
                }
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
            self.shared_views.source_bases.remove(&operation);
            self.shared_views
                .pending
                .retain(|(id, _, _)| *id != operation);
        } else {
            self.shared_views.seeding.remove(&scope);
            self.shared_views.initialized.insert(scope.clone());
            // The receipt admits only this scope. Other scopes retain their
            // own seed/baseline and pending edits until their receipts arrive.
            let cards = self.local_view_cards(&scope);
            let panes = self.local_view_panes(&scope);
            self.rebase_seeded_attachments(&scope, &panes);
            self.publish_projected_view_changes(LocalViews {
                scopes: vec![scope.clone()],
                cards: HashMap::from([(scope.clone(), cards)]),
                panes: HashMap::from([(scope, panes)]),
            });
        }
    }

    /// A seed's baseline is what this window saved, but the registry keeps the
    /// panes it already holds. A terminal attached while the seed was in flight
    /// has proved its same session under the running daemon, so that update is
    /// based on the registry's record. Based on the saved one it is refused, and
    /// the record then stays behind the attachment until another window writes.
    fn rebase_seeded_attachments(&mut self, scope: &Scope, local: &[Pane]) {
        let Some(view) = self
            .workspace_model
            .views
            .iter()
            .find(|view| view.scope == *scope)
        else {
            return;
        };
        let Some(baseline) = self.shared_views.baseline.get_mut(scope) else {
            return;
        };
        for seeded in baseline {
            let same_id = |pane: &&Pane| pane.id == seeded.id;
            let (Some(current), Some(shared)) =
                (local.iter().find(same_id), view.panes.iter().find(same_id))
            else {
                continue;
            };
            if current.kind != seeded.kind
                && current.scope == shared.scope
                && current.pending_terminal_start == shared.pending_terminal_start
                && attached_to_same_terminal(&current.kind, &shared.kind)
                && surface_attached_under(self.cards.get(&seeded.id), &current.kind)
            {
                seeded.kind = shared.kind.clone();
                seeded.pending_terminal_start = shared.pending_terminal_start;
                seeded.scope = shared.scope.clone();
            }
        }
    }

    /// A transfer can commit before a later metadata edit reaches the daemon.
    /// Keep that edit over the new owner and give its retry a distinct receipt:
    /// the original old-scope operation can still acknowledge a harmless no-op.
    pub(super) fn retarget_transferred_pane_updates(&mut self, acknowledged: Option<CardId>) {
        let moving: HashSet<_> = self
            .shared_views
            .requested_transfers
            .values()
            .copied()
            .chain(acknowledged)
            .collect();
        if moving.is_empty() {
            return;
        }
        let mut edits = Vec::new();
        let mut discarded = HashSet::new();
        let mut stale_actor = false;
        for (operation, scope, change) in &mut self.shared_views.pending {
            let Change::Update { before, pane } = change else {
                continue;
            };
            let card = pane.id;
            if !moving.contains(&card) {
                continue;
            }
            let mut owners = self
                .workspace_model
                .views
                .iter()
                .filter(|view| view.panes.iter().any(|candidate| candidate.id == card));
            let Some(owner) = owners.next() else {
                continue;
            };
            if owners.next().is_some() || owner.scope == *scope {
                continue;
            }
            let repository_for = |scope: &Scope| match scope {
                Scope::Channel(channel) => self
                    .workspace_model
                    .channel(*channel)
                    .map(|channel| channel.repository.clone()),
                Scope::Worktree { repository, .. } => Some(repository.clone()),
            };
            let source_verified = self.shared_views.source_bases.get(operation).is_some_and(
                |(identity, repository)| {
                    *identity == self.workspace_model.identity
                        && repository_for(scope).as_deref() == Some(repository.as_path())
                        && repository_for(&owner.scope).as_deref() == Some(repository.as_path())
                },
            );
            if !source_verified {
                // Unknown origin stays unknown: do not mint a retry whose
                // publication would capture unrelated current authority.
                discarded.insert(*operation);
                stale_actor = true;
                continue;
            }
            let current = owner.panes.iter().find(|pane| pane.id == card).unwrap();
            if crate::workspace_model::views::updates_source(before, pane)
                && !can_retry_source_fields(before, current)
            {
                before.kind = current.kind.clone();
                pane.kind = current.kind.clone();
                before.pending_terminal_start = current.pending_terminal_start;
                pane.pending_terminal_start = current.pending_terminal_start;
                before.scope = current.scope.clone();
                pane.scope = current.scope.clone();
                stale_actor = true;
            }
            if before == pane {
                discarded.insert(*operation);
                continue;
            }
            let source_basis = self.shared_views.source_bases.remove(operation);
            *operation = uuid::Uuid::new_v4();
            if let Some(source_basis) = source_basis {
                self.shared_views
                    .source_bases
                    .insert(*operation, source_basis);
            }
            *scope = owner.scope.clone();
            edits.push(ModelEdit::ChangeView {
                scope: scope.clone(),
                expected_revision: owner.revision,
                operation: *operation,
                change: change.clone(),
            });
        }
        self.shared_views
            .pending
            .retain(|(operation, _, _)| !discarded.contains(operation));
        self.shared_views
            .source_bases
            .retain(|operation, _| !discarded.contains(operation));
        for edit in edits {
            self.publish_model_edit(edit);
        }
        if stale_actor {
            self.notices.record(TransientNotice::warning(
                "Kept the current pane session because an incoming session update could not be verified"
            ));
        }
    }

    /// A full canonical snapshot already supplies view authority. A seed is
    /// needed only when local migration could change that view, not as a
    /// per-client handshake. Keep exact membership and field equality here:
    /// omitted defaults/retained cards and uncommitted intentions must still
    /// take the ordinary migration path.
    fn admit_existing_views_without_migration(&mut self) {
        self.admit_existing_views_in(None);
    }

    fn admit_existing_views_in(&mut self, selected: Option<&HashSet<Scope>>) {
        if self.workspace_model.identity == crate::workspace_model::NO_REGISTRY {
            return;
        }
        let candidates = self
            .workspace_model
            .views
            .iter()
            .enumerate()
            .filter(|(_, view)| {
                selected.is_none_or(|selected| selected.contains(&view.scope))
                    && !self.shared_views.initialized.contains(&view.scope)
                    && !self.shared_views.seeding.contains(&view.scope)
                    && !self.shared_views.blocked.contains(&view.scope)
                    && self
                        .shared_views
                        .baseline
                        .get(&view.scope)
                        .is_none_or(Vec::is_empty)
            })
            .map(|(index, view)| (index, view.scope.clone()))
            .collect::<Vec<_>>();
        if candidates.is_empty() {
            return;
        }
        // Eligibility must see an active strip's current order/focus, not the
        // preceding durable snapshot. This is one immutable eligibility pass;
        // canonical projection below gets its own fresh publication baseline.
        self.snapshot_workspace_scope();
        let projected = self.local_views_in(selected);
        let mut pending_scopes = HashSet::new();
        for (_, source, change) in &self.shared_views.pending {
            pending_scopes.insert(source.clone());
            if let Change::Transfer { destination, .. } = change {
                pending_scopes.insert(destination.clone());
            }
        }
        let candidate_count = candidates.len();
        let mut admitted = 0;
        for (index, scope) in candidates {
            if pending_scopes.contains(&scope)
                || matches!(&scope, Scope::Channel(id)
                    if self.channel_ui.was_deleted(*id) || self.channel_ui.is_deleting(*id))
            {
                continue;
            }
            let (Some(members), Some(panes)) =
                (projected.cards.get(&scope), projected.panes.get(&scope))
            else {
                continue;
            };
            if !members.iter().copied().eq(panes.iter().map(|pane| pane.id))
                || *panes != self.workspace_model.views[index].panes
            {
                continue;
            }
            let active = match &scope {
                Scope::Channel(id) => self.durable.active_channel == Some(*id),
                Scope::Worktree { path, .. } => {
                    self.durable.active_channel.is_none()
                        && self.active_workspace.as_ref() == Some(path)
                }
            };
            // Matching saved descriptors do not establish that an active
            // strip already shows this scope. Preserve its current focus until
            // ordinary migration settles a still-transitioning presentation.
            if active
                && !panes.is_empty()
                && !self
                    .workspace
                    .focused()
                    .is_some_and(|id| members.contains(&id))
            {
                continue;
            }
            self.shared_views.attached.extend(
                panes
                    .iter()
                    .filter(|pane| matches!(pane.kind, CardKind::Chat(_)))
                    .map(|pane| pane.id),
            );
            self.shared_views
                .baseline
                .insert(scope.clone(), panes.clone());
            self.shared_views.initialized.insert(scope);
            admitted += 1;
        }
        tracing::debug!(
            admitted,
            candidates = candidate_count,
            "Admitted existing workspace views without migration"
        );
    }

    /// A repository load changes ownership boundaries, not registry authority.
    /// Follow pane ownership across local, baseline and canonical scopes so a
    /// discovered child root, transfer or rekeyed alias settles both ends.
    fn repository_view_scopes(
        &self,
        repository: &Path,
        additional: &HashSet<Scope>,
    ) -> HashSet<Scope> {
        let discovery = self.discover_view_scopes();
        let projects = self
            .sidebar_model
            .projects
            .iter()
            .filter(|project| project.repository == repository)
            .map(|project| project.id)
            .collect::<HashSet<_>>();
        let channels = self
            .durable
            .channels
            .iter()
            .filter(|channel| projects.contains(&channel.project_id))
            .map(|channel| channel.id)
            .chain(
                self.workspace_model
                    .channels
                    .iter()
                    .filter(|channel| channel.repository == repository)
                    .map(|channel| channel.id),
            )
            .collect::<HashSet<_>>();
        let belongs = |scope: &Scope| match scope {
            Scope::Worktree {
                repository: owner, ..
            } => owner == repository,
            Scope::Channel(id) => channels.contains(id),
        };
        let mut selected = discovery
            .scopes
            .iter()
            .chain(self.workspace_model.views.iter().map(|view| &view.scope))
            .filter(|scope| belongs(scope))
            .cloned()
            .collect::<HashSet<_>>();
        let mut owners = HashMap::<CardId, HashSet<Scope>>::new();
        for channel in &self.durable.channels {
            for id in &channel.cards {
                owners
                    .entry(*id)
                    .or_default()
                    .insert(Scope::Channel(channel.id));
            }
        }
        let mut scopes_by_path = HashMap::<&Path, Vec<&Scope>>::new();
        for scope in &discovery.scopes {
            if let Scope::Worktree { path, .. } = scope {
                scopes_by_path
                    .entry(path.as_path())
                    .or_default()
                    .push(scope);
            }
        }
        for (card, path) in discovery.worktree_cards {
            if card.existence.is_open()
                && !card.kind.is_utility()
                && let Some(scopes) = scopes_by_path.get(path)
            {
                owners
                    .entry(card.id)
                    .or_default()
                    .extend(scopes.iter().map(|scope| (*scope).clone()));
            }
        }
        for (scope, panes) in &self.shared_views.baseline {
            for pane in panes {
                owners.entry(pane.id).or_default().insert(scope.clone());
            }
        }
        let owned = self
            .workspace_model
            .views
            .iter()
            .flat_map(|view| view.panes.iter().map(|pane| pane.id))
            .collect::<HashSet<_>>();
        let alias_candidates = self
            .durable
            .cards
            .iter()
            .filter(|card| card.existence.is_open() && !owned.contains(&card.id))
            .collect::<Vec<_>>();
        for view in &self.workspace_model.views {
            for pane in &view.panes {
                owners
                    .entry(pane.id)
                    .or_default()
                    .insert(view.scope.clone());
                // A canonical rekey can retire an unowned local alias in a
                // different scope; its source baseline must settle too.
                for alias in alias_candidates
                    .iter()
                    .filter(|card| views::same_resource(&card.kind, &pane.kind))
                {
                    let aliases = owners.get(&alias.id).cloned().unwrap_or_default();
                    owners.entry(pane.id).or_default().extend(aliases);
                }
            }
        }
        for (_, scope, change) in &self.shared_views.pending {
            let id = match change {
                Change::Insert { pane, .. }
                | Change::Update { pane, .. }
                | Change::InsertIfEmpty { pane } => pane.id,
                Change::Remove { id } | Change::Move { id, .. } | Change::Transfer { id, .. } => {
                    *id
                }
            };
            let entries = owners.entry(id).or_default();
            entries.insert(scope.clone());
            if let Change::Transfer { destination, .. } = change {
                entries.insert(destination.clone());
            }
        }
        selected.extend(additional.iter().cloned());
        let mut cards_by_scope = HashMap::<&Scope, Vec<CardId>>::new();
        for (id, scopes) in &owners {
            for scope in scopes {
                cards_by_scope.entry(scope).or_default().push(*id);
            }
        }
        let mut queued = selected.iter().cloned().collect::<VecDeque<_>>();
        let mut visited = HashSet::new();
        while let Some(scope) = queued.pop_front() {
            for id in cards_by_scope.get(&scope).into_iter().flatten() {
                if !visited.insert(*id) {
                    continue;
                }
                for owner in &owners[id] {
                    if selected.insert(owner.clone()) {
                        queued.push_back(owner.clone());
                    }
                }
            }
        }
        selected
    }

    pub(super) fn reconcile_shared_views(&mut self) -> Task<Message> {
        self.reconcile_shared_views_in(None)
    }

    pub(super) fn reconcile_shared_views_for_repository(
        &mut self,
        repository: &Path,
        adopted: &HashSet<Scope>,
    ) -> Task<Message> {
        self.reconcile_shared_views_in(Some((repository, adopted)))
    }

    fn reconcile_shared_views_in(
        &mut self,
        repository: Option<(&Path, &HashSet<Scope>)>,
    ) -> Task<Message> {
        self.publish_unpublished_local_chats();
        // A watcher snapshot can announce the destination before the explicit
        // transfer response arrives. Rebase before either snapshot projects.
        self.retarget_transferred_pane_updates(None);
        let selected = repository
            .map(|(repository, adopted)| self.repository_view_scopes(repository, adopted));
        let selected = selected.as_ref();
        self.shared_views.prune_restoration_requests(&self.durable);
        self.admit_existing_views_in(selected);
        // A delayed tombstone must not borrow a newer local actor's identity.
        // Record only raw authoritative panes, never the pending overlay below.
        let local_cards = self
            .durable
            .cards
            .iter()
            .filter(|card| card.existence.is_open())
            .map(|card| card.id)
            .collect::<HashSet<_>>();
        self.shared_views
            .observed_chats
            .retain(|id, _| local_cards.contains(id));
        self.shared_views
            .closed_chats
            .retain(|id, _| local_cards.contains(id));
        let mut seen = HashSet::new();
        let mut ambiguous = HashSet::new();
        for pane in self
            .workspace_model
            .views
            .iter()
            .flat_map(|view| &view.panes)
            .filter(|pane| local_cards.contains(&pane.id))
        {
            if !seen.insert(pane.id) {
                ambiguous.insert(pane.id);
            }
            if let CardKind::Chat(session) = &pane.kind {
                if self.shared_views.observed_chats.get(&pane.id) != Some(session) {
                    self.shared_views
                        .observed_chats
                        .insert(pane.id, session.clone());
                }
            } else {
                self.shared_views.observed_chats.remove(&pane.id);
            }
        }
        for id in ambiguous {
            self.shared_views.observed_chats.remove(&id);
        }
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
        // Snapshot each scope's ordered overlay once. Projection below mutates
        // local state, so this pass owns its changes rather than borrowing the
        // pending queue across those mutations.
        let mut pending_by_scope = HashMap::<Scope, Vec<Change>>::new();
        for (_, scope, change) in &self.shared_views.pending {
            if !matches!(change, Change::Transfer { .. }) {
                pending_by_scope
                    .entry(scope.clone())
                    .or_default()
                    .push(change.clone());
            }
        }
        for view in &mut views {
            for change in pending_by_scope.get(&view.scope).into_iter().flatten() {
                let _ = view.apply(change);
            }
        }
        let owned = views
            .iter()
            .flat_map(|view| view.panes.iter().map(|pane| pane.id))
            .collect::<HashSet<_>>();
        self.prepare_shared_aliases_in(&views, &owned, selected);
        // Root discovery is immutable during this projection, so index it once
        // over owned paths that outlive each mutable view update. Keep only
        // owned card paths too, and refresh the panes each view changes before
        // visiting the next one.
        let root_paths = self.worktree_root_paths();
        let roots = worktree_ui::WorktreeRoots::new(root_paths.iter().map(PathBuf::as_path));
        let mut local_worktrees = CardWorktrees::default();
        for card in self
            .durable
            .cards
            .iter()
            .filter(|card| card.existence.is_open())
        {
            if let Some(path) = self.unchanneled_card_worktree_indexed(card, &roots) {
                local_worktrees.insert(card.id, path.to_owned());
            }
        }
        let mut projected_scopes = Vec::new();
        for mut view in views {
            if selected.is_some_and(|selected| !selected.contains(&view.scope)) {
                continue;
            }
            if self.shared_views.blocked.contains(&view.scope) {
                continue;
            }
            if !self.shared_views.initialized.contains(&view.scope)
                || self.shared_views.seeding.contains(&view.scope)
            {
                continue;
            }
            for change in pending_by_scope.get(&view.scope).into_iter().flatten() {
                if let Err(error) = view.apply(change) {
                    self.status = Some(TransientNotice::error(error));
                }
            }
            let current = match &view.scope {
                Scope::Channel(_) => self.local_view_cards(&view.scope),
                // Most views are historical: no card resolves to them at all.
                Scope::Worktree { path, .. } if !local_worktrees.holds(path) => Vec::new(),
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
                                && local_worktrees.get(card.id) == Some(path)
                        })
                        .map(|card| card.id)
                        .collect()
                }
            };
            tasks.push(self.project_shared_view_with_cards(&view, &owned, current));
            if !view.panes.is_empty() {
                for pane in &view.panes {
                    if let Some(path) = self
                        .card_state(pane.id)
                        .and_then(|card| self.unchanneled_card_worktree_indexed(card, &roots))
                    {
                        local_worktrees.insert(pane.id, path.to_owned());
                    } else {
                        local_worktrees.remove(pane.id);
                    }
                }
            }
            projected_scopes.push(view.scope);
        }
        self.shared_views.applying = false;
        // Capture after the active strip snapshot, then share this immutable
        // pass between settled baselines and publication. Uninitialized scopes
        // and local/default intentions still use the ordinary publisher.
        // Held reported titles stay held: publishing them on every view
        // acknowledgment would publish each title again as its own echo.
        self.save_workspace_with_view_publication(|app| {
            let projected = app.local_views_in(selected);
            for scope in projected_scopes {
                app.shared_views.baseline.insert(
                    scope.clone(),
                    projected.panes.get(&scope).cloned().unwrap_or_default(),
                );
            }
            app.publish_projected_view_changes(projected);
        });
        if let Some(path) = self.shared_views.deferred_default.take() {
            if self.active_workspace.as_ref() == Some(&path) && self.workspace.cards().is_empty() {
                tasks.push(self.add_default_pane(path, settings::DefaultPane::Terminal));
            }
        }
        self.reproject_notification_unread();
        tasks.push(self.release_held_terminals());
        // Restored shared panes queued for an attach wait for the registry.
        tasks.push(self.attach_next_restored_agent());
        Task::batch(tasks)
    }

    fn prepare_shared_aliases(&mut self, views: &[View], owned: &HashSet<CardId>) {
        self.prepare_shared_aliases_in(views, owned, None);
    }

    fn prepare_shared_aliases_in(
        &mut self,
        views: &[View],
        owned: &HashSet<CardId>,
        selected: Option<&HashSet<Scope>>,
    ) {
        self.shared_views.blocked = blocked_managed_view_scopes(&self.workspace_model, views);
        let mut attention_owners: HashMap<_, usize> = HashMap::new();
        for pane in views.iter().flat_map(|view| &view.panes) {
            if let Some(resource) = attention_resource(&pane.kind) {
                *attention_owners.entry(resource).or_default() += 1;
            }
        }
        // Decide which whole views can project before retiring any local alias.
        // Otherwise a later blocked pane could leave an earlier adoption without
        // the canonical descriptor required by its persistence checkpoint.
        let alias_candidates = self
            .durable
            .cards
            .iter()
            .filter(|card| card.existence.is_open() && !owned.contains(&card.id))
            .collect::<Vec<_>>();
        let mut adoptions = Vec::new();
        for view in views {
            for shared in &view.panes {
                let aliases = alias_candidates
                    .iter()
                    .filter(|card| views::same_resource(&card.kind, &shared.kind))
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
                        for scope in self.local_scopes_listing(alias) {
                            if scope != view.scope {
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
                        for scope in self.local_scopes_listing(alias) {
                            self.shared_views.blocked.insert(scope);
                        }
                        self.status = Some(TransientNotice::warning(
                            "A shared pane has a new identity. Finish or discard its local draft before adopting it.",
                        ));
                        continue;
                    }
                    if selected.is_none_or(|selected| selected.contains(&view.scope)) {
                        adoptions.push((view.scope.clone(), alias, shared.clone()));
                    }
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
            // ID equality alone is not a conversation proof. Preserve unread
            // only for the exact logical/native source being rekeyed; a
            // replacement process/thread must not inherit an old actor's read
            // state. Runtime activity awaits its own fresh observation.
            if attention_resource(&shared.kind)
                .is_some_and(|resource| attention_owners.get(&resource) == Some(&1))
                && self
                    .card_state(alias)
                    .is_some_and(|card| same_attention_update_source(&card.kind, &shared.kind))
            {
                self.sidebar_state.transfer_card_update(alias, shared.id);
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
        mut current: Vec<CardId>,
    ) -> Task<Message> {
        #[cfg(test)]
        self.shared_views
            .view_projection_count
            .set(self.shared_views.view_projection_count.get() + 1);
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
        let mut retained_upgrades = Vec::new();
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
            // A refused first publication keeps its chat, started or not,
            // local until the user closes it. Persisted local-only custody
            // survives UI reopen, but never overrides canonical retirement.
            if self.shared_views.retained.contains(&id)
                && self
                    .card_state(id)
                    .is_some_and(|card| matches!(&card.kind, CardKind::Chat(_)))
                && !self.pane_retired(id, view)
            {
                continue;
            }
            let (pending, saved_copy) = self.preserve_pending_shared_upgrade(id);
            tasks.push(pending);
            retained_upgrades.extend(saved_copy);
            if matches!(self.cards.get(&id), Some(CardRuntime::Repository(runtime)) if runtime.is_dirty() || runtime.is_saving())
                || self.shared_chat_has_local_work(id)
            {
                // Each later read finds the same local pane absent again;
                // only the read that first kept it here reports a close.
                let newly_local = !self.shared_views.retained.contains(&id);
                self.retain_local_pane(id);
                if view.retired.contains(&id)
                    && !self.chat_start_pending(id)
                    && let Some(source) = self.shared_views.observed_chats.get(&id)
                    && matches!(self.card_state(id).map(|card| &card.kind), Some(CardKind::Chat(session)) if super::chat_ui::closed_draft_source_matches(source, session))
                {
                    if self.shared_views.closed_chats.get(&id) != Some(source) {
                        let source = source.clone();
                        self.shared_views.closed_chats.insert(id, source);
                    }
                    self.sync_closed_chat_draft(id);
                }
                // Only a canonical removal retires a pane. One merely absent
                // was never shared, so nothing else closed it.
                if newly_local {
                    self.status = Some(TransientNotice::warning(if view.retired.contains(&id) {
                        "A pane was closed in another client. Its unsaved edits are retained here; save or discard them before closing it.".to_string()
                    } else {
                        self.kept_here_notice(id, "Its unsaved edits stay here.")
                    }));
                }
                continue;
            }
            if self.absence_closes_pane(id, view) {
                self.detach_shared_pane(id);
                continue;
            }
            // Nobody closed this pane: a snapshot only says what the registry
            // holds, and whole-model updates re-project every view whatever
            // changed. Anything still running here stays where it is.
            if !self.shared_views.retained.contains(&id) {
                tracing::warn!(card = %id, scope = ?view.scope, "keeping a pane the shared view has neither listed nor retired");
                self.status = Some(TransientNotice::warning(
                    self.kept_here_notice(id, "It keeps working here."),
                ));
            }
            self.retain_local_pane(id);
        }
        current.extend(retained_upgrades);
        for shared in &view.panes {
            self.shared_views.closed_chats.remove(&shared.id);
            self.sync_closed_chat_draft(shared.id);
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
                // A pane's project is window-local and never shared. When the
                // echo describes this window's own pane, its project stands:
                // a file pane's folder is rarely a project root, and a
                // different project would replace the live viewer, losing
                // the line a link asked it to open at.
                let local = self
                    .card_state(projected.id)
                    .and_then(|card| match &card.kind {
                        CardKind::Repository(local)
                            if views::shared_kind(&card.kind)
                                == views::shared_kind(&shared.kind) =>
                        {
                            Some(local.project)
                        }
                        _ => None,
                    });
                if let Some(project) = local
                    .or_else(|| self.project_for_repository(&repository.repository))
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
            // A terminal this window restored as its own saved terminal can be
            // listed in a view later, through its own seed or once its folder
            // is opened as a project. Shared, it may only attach, so it leaves
            // the saved presentation, which no longer owns it, and hydrates on
            // the attach-only path instead of loading with nothing to start it.
            self.shared_views.attached.insert(pane.id);
            let leaves_saved = matches!(pane.kind, CardKind::Terminal(_))
                && self.saved_terminals.entries.remove(&pane.id).is_some();
            if self.shared_upgrade_target_changed(pane) {
                let (pending, saved_copy) = self.preserve_pending_shared_upgrade(pane.id);
                tasks.push(pending);
                current.extend(saved_copy);
            }
            self.prepare_shared_chat_upgrade(pane);
            if self.chat_awaiting_admission(pane.id) {
                let (task, saved_copy) = self.admit_shared_chat(pane);
                tasks.push(task);
                current.extend(saved_copy);
            }
            match &pane.user_title {
                Some(title) => {
                    // A user's name takes precedence over a reported title,
                    // which must not be published back with it.
                    self.reported_title_save.cards.remove(&pane.id);
                    self.chrome_ui.user_titles.insert(pane.id, title.clone());
                }
                None => {
                    self.chrome_ui.user_titles.remove(&pane.id);
                }
            }
            let existing = self.card_state(pane.id).cloned();
            // The exit this window saw, read before a readmission below can
            // mark the card unstarted.
            let observed_exit = self.ended_terminal(pane.id).cloned();
            // A restored shared terminal may have waited for the registry
            // while lifecycle hydration was admitted. Once its authoritative
            // descriptor arrives, admit that exact identity again.
            let no_start = !self
                .terminal_starts
                .lock()
                .expect("terminal starts lock")
                .contains_key(&pane.id);
            let waiting = matches!(self.cards.get(&pane.id), Some(CardRuntime::Loading))
                && existing.is_some()
                && matches!(self.residency(pane.id), Residency::Hydrating { .. });
            let failed_pending = pane.pending_terminal_start
                && matches!(self.cards.get(&pane.id), Some(CardRuntime::Failed(_)));
            if matches!(pane.kind, CardKind::Terminal(_))
                && ((no_start && (waiting || failed_pending))
                    || (leaves_saved
                        && matches!(self.cards.get(&pane.id), Some(CardRuntime::Loading))))
            {
                let exposure = self.exposure(pane.id);
                self.readmit_held_terminal(pane.id, exposure);
            }
            match existing {
                Some(card) if card.existence.is_open() => {
                    // The daemon can publish the conversation a resume in this
                    // window started before that start replies. The start owns
                    // the card's runtime, with its transcript and draft, and
                    // puts the conversation in the card itself once it is saved.
                    let resuming = matches!(&pane.kind, CardKind::Chat(session) if self.chat_resume_in_flight(pane.id, session));
                    if card.kind != pane.kind
                        && matches!(self.cards.get(&pane.id), Some(CardRuntime::Repository(runtime)) if runtime.is_dirty() || runtime.is_saving())
                    {
                        self.status = Some(TransientNotice::warning(
                            "Another client changed this pane. Save or discard its unsaved edits before adopting that change.",
                        ));
                        continue;
                    }
                    if card.kind != pane.kind && !resuming {
                        // Runtime metadata can change, but an exact live actor
                        // remains attached unless the session identity changes.
                        let same_session = match (&card.kind, &pane.kind) {
                            (CardKind::Terminal(a), CardKind::Terminal(b)) => a.id == b.id,
                            (CardKind::Chat(a), CardKind::Chat(b)) => a.id == b.id,
                            (a, b) => a == b,
                        };
                        if let (CardKind::Chat(saved), CardKind::Chat(canonical)) =
                            (&card.kind, &pane.kind)
                        {
                            self.adopt_shared_chat_identity(pane.id, saved, canonical);
                        }
                        // A draft choosing or loading files follows a
                        // conversation started from it once that ends.
                        let inserting = matches!(&pane.kind, CardKind::Chat(canonical) if !canonical.is_unstarted())
                            && self.chat_draft_inserting(pane.id);
                        if !same_session && !inserting {
                            // A held title belongs to the replaced session.
                            self.reported_title_save.cards.remove(&pane.id);
                            self.forget_peer_started(pane.id);
                            // Text the reader is composing in a draft was never
                            // sent anywhere, so the chat that replaces the draft
                            // opens with it: another unstarted draft, or the
                            // conversation another client started from it, even
                            // when the reader never saw the intermediate draft.
                            // What the runtime shows decides, not the stored
                            // descriptor, which may already name a started
                            // conversation. Text written to a started
                            // conversation is not handed to a different one.
                            if matches!(pane.kind, CardKind::Chat(_))
                                && matches!(self.cards.get(&pane.id),
                                    Some(CardRuntime::Chat(runtime)) if runtime.session.is_unstarted())
                            {
                                let started = matches!(&pane.kind, CardKind::Chat(canonical) if !canonical.is_unstarted());
                                self.carry_unsent_chat(pane.id, started);
                            }
                            self.cards.remove(&pane.id);
                            self.lifecycle.remove(pane.id);
                            // Card identity survives a session replacement, but
                            // the previous actor's attention does not. This is
                            // retirement, not evidence that its news was read.
                            self.sidebar_state.forget_card_attention(pane.id);
                        }
                    }
                    let mut kind = pane.kind.clone();
                    preserve_observed_terminal_exit(observed_exit.as_ref(), &mut kind);
                    // A reported title waiting for its save is newer than the
                    // shared one, which it replaces when that save publishes it.
                    let reported = self.reported_title_save.cards.contains(&pane.id);
                    // Writing a card copies it while a save still holds it,
                    // so only a card the view changes is written.
                    let changed = self.card_state(pane.id).is_some_and(|current| {
                        (!reported && current.title != pane.title)
                            || current.width_fraction != pane.width_fraction
                            || (!resuming && current.kind != kind)
                    });
                    if changed && let Some(current) = self.card_state_mut(pane.id) {
                        if !reported {
                            current.title = pane.title.clone();
                        }
                        current.width_fraction = pane.width_fraction;
                        if !resuming {
                            current.kind = kind;
                        }
                    }
                    // Publishing a locally opened chat can echo its pane
                    // before the provider handshake creates a runtime. Its
                    // exact in-flight attempt already owns that attachment;
                    // replacing it would discard the first start's reply.
                    // A different canonical descriptor still needs attachment.
                    let starting = matches!(&pane.kind, CardKind::Chat(session) if self.chat_start_matches(pane.id, session));
                    if resuming {
                        // Its start attaches the card.
                    } else if !self.cards.contains_key(&pane.id) && !starting {
                        tasks.push(self.attach_shared_pane(pane));
                    } else if let CardKind::Chat(session) = &pane.kind {
                        if self.release_chat_returned_to_draft(pane.id, session)
                            || self.release_draft_started_elsewhere(pane.id, session)
                        {
                            tasks.push(self.attach_shared_pane(pane));
                        } else {
                            tasks.push(self.adopt_shared_draft_session(pane.id, session));
                        }
                    }
                }
                _ => {
                    // A closed card the view reopens may hold another
                    // conversation than the one it last saved.
                    if let (Some(CardKind::Chat(saved)), CardKind::Chat(canonical)) = (
                        self.card_state(pane.id).map(|card| card.kind.clone()),
                        &pane.kind,
                    ) {
                        self.adopt_shared_chat_identity(pane.id, &saved, canonical);
                    }
                    let card = CardState {
                        id: pane.id,
                        generation: self.durable.generation,
                        title: pane.title.clone(),
                        width_fraction: pane.width_fraction,
                        existence: Existence::Open,
                        kind: pane.kind.clone(),
                    };
                    let durable = Arc::make_mut(&mut self.durable);
                    if let Some(existing) = durable.cards.iter_mut().find(|card| card.id == pane.id)
                    {
                        *existing = card.into();
                    } else {
                        durable.cards.push(card);
                    }
                    // A pane arriving from another window starts here unseen
                    // and unstarted, whatever this window last held for it.
                    self.set_presence(pane.id, Exposure::Offscreen, Residency::Cold);
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
        Arc::make_mut(&mut self.durable)
            .cards
            .arrange_within(&visible);
        if active {
            tasks.push(self.project_shared_strip(&view.panes, &visible));
        }
        if !self.shared_views.applying {
            self.persist();
        }
        Task::batch(tasks)
    }

    /// Attaches draft `card` to the conversation another window started in the
    /// shared view while the card could not let its draft go.
    pub(super) fn attach_draft_started_elsewhere(
        &mut self,
        card: CardId,
        session: &crate::chat::Session,
    ) -> Task<Message> {
        let Some(pane) = self
            .workspace_model
            .views
            .iter()
            .flat_map(|view| &view.panes)
            .find(|pane| pane.id == card && pane.kind == CardKind::Chat(session.clone()))
            .cloned()
        else {
            return Task::none();
        };
        if !self.release_draft_started_elsewhere(card, session) {
            return Task::none();
        }
        self.attach_shared_pane(&pane)
    }

    /// Records that chat card `card`, which this window holds as `saved`,
    /// now shows the shared view's `canonical` conversation.
    ///
    /// Storage refuses a snapshot that re-points a chat card at another
    /// conversation, or changes what its conversation is bound to, unless the
    /// change is recorded explicitly. The client that made the change recorded
    /// it in its own store; this window, which only follows the canonical
    /// view, records it in its own before the next save (#2176, #2349,
    /// #2402).
    ///
    /// A stopgap for windows that keep their own copy of each card: once the
    /// daemon's store is the only one holding card content, this and
    /// `StateStore::adopt_chat_identity` have nothing left to reconcile.
    pub(super) fn adopt_shared_chat_identity(
        &mut self,
        card: CardId,
        saved: &crate::chat::Session,
        canonical: &crate::chat::Session,
    ) {
        use std::collections::hash_map::Entry;
        // While this window's own first send is saving, storage is about to
        // hold the conversation it queued rather than the draft memory still
        // shows; a peer's start that wins replaces that one. A refused send
        // leaves the draft, and the writer then expects that instead.
        let stored = self.draft_start_candidate(card).unwrap_or(saved).clone();
        match self.shared_views.chat_adoptions.entry(card) {
            // Storage decides at the write whether anything is left to adopt,
            // including a return to the conversation it already holds.
            Entry::Occupied(mut entry) => entry.get_mut().canonical = canonical.clone(),
            Entry::Vacant(entry)
                if crate::persistence::chat_identity_change(&stored, canonical).is_some() =>
            {
                entry.insert(crate::session::ChatIdentityAdoption {
                    card,
                    expected: stored.id,
                    canonical: canonical.clone(),
                });
            }
            Entry::Vacant(_) => {}
        }
    }

    fn attach_shared_pane(&mut self, pane: &Pane) -> Task<Message> {
        // Saved terminal outcomes are presentation, never a hydration request.
        // In particular, RelaunchAvailable retains an explicit restart action.
        if matches!(&pane.kind, CardKind::Terminal(session) if terminal_ended(session)) {
            self.cards.insert(pane.id, CardRuntime::Completed);
            self.lifecycle.remove(pane.id);
            self.set_residency(pane.id, Residency::Hibernated);
            return Task::none();
        }
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
            kind @ (CardKind::Settings
            | CardKind::Accounts
            | CardKind::Automations
            | CardKind::Work) => {
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
        self.set_residency(pane.id, Residency::Hot);
        task
    }

    /// `view` carries this window's pending edits over the canonical one, so a
    /// local close counts here as well as a peer's.
    fn pane_retired(&self, id: CardId, view: &View) -> bool {
        view.retired.contains(&id)
            || self
                .workspace_model
                .views
                .iter()
                .any(|view| view.retired.contains(&id))
    }

    /// Whether a pane missing from its shared view should close here. Only a
    /// recorded removal says someone closed it. The automatic panes below have
    /// no actor and no work; a view that gained content simply replaces them.
    fn kept_here_notice(&self, id: CardId, consequence: &str) -> String {
        let title = self
            .card_state(id)
            .map(|card| card.title.as_str())
            .filter(|title| !title.is_empty())
            .unwrap_or("This pane");
        format!(
            "\u{201c}{title}\u{201d} is open only in this window, so your other windows can't see it. {consequence}"
        )
    }

    fn absence_closes_pane(&self, id: CardId, view: &View) -> bool {
        self.pane_retired(id, view)
            || self.chat_is_unused_default(id)
            || self.shared_views.defaults.contains_key(&id)
            // A terminal waits for its creation to be accepted before starting,
            // so a refused one has no process to lose and would only sit on
            // Loading forever.
            || self.durable.pending_terminal_creations.contains(&id)
            || !self
                .card_state(id)
                .is_some_and(|card| card.existence.is_open())
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
        self.sidebar_state.forget_card_attention(id);
    }

    /// Refresh presentation after an exact managed owner changes on first model
    /// sync. The existing runtime remains attached; no snapshot of the old strip
    /// may publish the pane back into its stale original channel.
    pub(super) fn refresh_managed_display_strip(&mut self) -> Task<Message> {
        let scope = self.durable.active_channel.map(Scope::Channel).or_else(|| {
            self.active_workspace
                .as_deref()
                .and_then(|path| self.worktree_view_scope(path))
        });
        let Some(scope) = scope else {
            return Task::none();
        };
        let panes = self.local_view_panes(&scope);
        let visible = self.local_view_cards(&scope);
        self.project_shared_strip(&panes, &visible)
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
            self.set_exposure(id, Exposure::Offscreen);
            if self.workspace.request_close(id).is_ok() {
                if let Ok(update) = self.workspace.commit_close(id) {
                    updates.push(update);
                }
            }
        }
        for (index, pane) in panes.iter().enumerate() {
            if self.workspace.card(pane.id).is_none() {
                // A held reported title is newer than the shared one.
                let held = self
                    .reported_title_save
                    .cards
                    .contains(&pane.id)
                    .then(|| self.card_state(pane.id).map(|card| card.title.as_str()))
                    .flatten();
                if let Ok(card) = WorkspaceCard::new(
                    pane.id,
                    pane.user_title.as_deref().or(held).unwrap_or_else(|| {
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
                // A held reported title is newer than the shared one.
                if pane.user_title.is_some() || !self.reported_title_save.cards.contains(&pane.id) {
                    let _ = self.workspace.rename_card(
                        pane.id,
                        pane.user_title
                            .as_deref()
                            .unwrap_or_else(|| {
                                browser_host::presentation_title(&pane.kind, &pane.title)
                            })
                            .to_owned(),
                    );
                }
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
                .map(|update| self.apply_viewport(update, false, DurableChange::None))
                .chain(std::iter::once(lifecycle)),
        )
    }
}

// A layout snapshot can predate a runtime exit. Missing recipe metadata cannot
// revoke a settled exit for that exact terminal; new identities and live host
// proofs still adopt normally, as do the incoming launch and other metadata.
/// `attached` holds a complete live proof for the terminal `shared` records as
/// still running. An ended record is newer than any attachment made to it.
fn attached_to_same_terminal(attached: &CardKind, shared: &CardKind) -> bool {
    matches!(
        (attached, shared),
        (CardKind::Terminal(attached), CardKind::Terminal(shared))
            if attached.id == shared.id
                && shared.continuity.live_proof().is_some()
                && matches!(attached.continuity, TerminalContinuity::Live { .. })
                && attached
                    .continuity
                    .live_proof()
                    .is_some_and(|proof| proof.is_complete())
    )
}

/// Whether a card's surface is attached, now, under the proof its record
/// claims. A start returns once attached and hibernating keeps the connection,
/// so no current attachment means that connection ended, and the proof may be
/// older than the registry's.
fn surface_attached_under(runtime: Option<&CardRuntime>, kind: &CardKind) -> bool {
    let (Some(CardRuntime::Terminal(surface)), CardKind::Terminal(session)) = (runtime, kind)
    else {
        return false;
    };
    matches!(surface.current_attachment(), Ok(Some(attached))
        if session.continuity.live_proof().as_ref() == Some(&attached))
}

/// Carries the exit this window saw, `observed` from
/// [`Cagents::ended_terminal`], into a shared descriptor that does not have
/// one yet.
fn preserve_observed_terminal_exit(observed: Option<&TerminalSession>, incoming: &mut CardKind) {
    let Some(observed) = observed else {
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

fn attention_resource(kind: &CardKind) -> Option<(u8, TerminalId)> {
    match kind {
        CardKind::Chat(session) => Some((0, session.id)),
        CardKind::Terminal(session) => Some((1, session.id)),
        _ => None,
    }
}

/// Volatile alias unread requires the same observed conversation/source.
/// Persisted journal notices use a separate, stable logical Session identity:
/// an unseen notice can still restore generic unread after a native reattach.
/// Neither route supplies current activity/fate or execution/cleanup authority.
fn same_attention_update_source(left: &CardKind, right: &CardKind) -> bool {
    match (left, right) {
        (CardKind::Chat(a), CardKind::Chat(b)) => {
            a.proof.as_ref().is_some_and(HostSessionProof::is_complete)
                && b.proof.as_ref().is_some_and(HostSessionProof::is_complete)
                && a.launch.session_id.as_ref().is_some_and(|session| {
                    !session.is_empty() && b.launch.session_id.as_ref() == Some(session)
                })
                && crate::agent_execution::same_chat_process(a, b)
        }
        (CardKind::Terminal(a), CardKind::Terminal(b)) => {
            a.continuity
                .live_proof()
                .is_some_and(|proof| proof.is_complete())
                && b.continuity
                    .live_proof()
                    .is_some_and(|proof| proof.is_complete())
                && crate::agent_execution::same_terminal_process(a, b)
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    mod history_start;
    mod published_card_edits;
    mod startup_admission;
    mod startup_terminals;

    use super::*;
    use crate::model::{
        BrowserId, BrowserRestoreFidelity, BrowserSession, ChannelId, ProjectChannel, ProjectId,
    };
    /// A report that a pane's conversation could not be reopened goes with
    /// the pane, whichever path removes it; here, another client closing it.
    #[test]
    fn a_pane_closed_elsewhere_takes_its_reopen_failure_with_it() {
        let mut app = super::super::tests::context_inspector_test_app(true);
        let card = app.workspace.focused().unwrap();
        let report = |card: CardId| {
            TransientNotice::error("Could not reopen a conversation.")
                .with_details(format!("{card:?}"))
                .with_key(NoticeKey::ChatAttach(card))
        };
        app.notices.record(report(card));
        app.notices.record(report(CardId::new()));
        // Any message will do: every update ends by checking.
        let _ = app.update(Message::DismissNotice(u64::MAX));
        assert_eq!(
            app.notices
                .recent()
                .map(|entry| entry.notice.key.clone())
                .collect::<Vec<_>>(),
            [Some(NoticeKey::ChatAttach(card))],
            "an open pane keeps its report; one that is gone has none"
        );

        app.detach_shared_pane(card);
        let _ = app.update(Message::DismissNotice(u64::MAX));
        assert!(app.notices.is_empty());
    }

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
        channel_move_fixture_with_kind(None)
    }

    fn channel_move_fixture_with_kind(
        kind: Option<CardKind>,
    ) -> (Cagents, ChannelId, ChannelId, Pane) {
        let (mut app, source, destination, mut pane) = fixture();
        if let Some(kind) = kind {
            pane.kind = kind;
        }
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
        if let CardKind::Terminal(session) = &pane.kind {
            // Projection dispatches an attach before returning its Task. This
            // fixture intentionally never polls that worker, so deliver its
            // explicit failed completion through the actual reducer before
            // testing a settled pane. Do not bypass the production start fence
            // or pretend that an actual terminal attachment succeeded.
            assert!(app.terminal_start_in_flight(pane.id));
            drop(app.update(Message::TerminalStarted {
                card: pane.id,
                terminal: session.id,
                result: Err("fixture terminal attach was intentionally not executed".into()),
            }));
            assert!(!app.terminal_start_in_flight(pane.id));
            assert!(matches!(
                app.cards.get(&pane.id),
                Some(CardRuntime::Failed(_))
            ));
            assert_eq!(app.card_state(pane.id).unwrap().kind, pane.kind);
        }
        (app, source, destination, pane)
    }

    fn edit_moving_terminal(app: &mut Cagents, card: CardId, source: ChannelId) -> Pane {
        use super::super::chrome_ui::Action;
        drop(app.update_chrome(Action::Rename(card)));
        drop(app.update_chrome(Action::RenameChanged("Renamed while moving".into())));
        drop(app.update_chrome(Action::RenameSubmit));
        app.workspace.resize_viewport(1000.0);
        drop(app.update(Message::ResizeCard { card, width: 640.0 }));
        drop(app.update(Message::ResizeCardFinished(card)));
        let CardKind::Terminal(session) = &mut app.card_state_mut(card).unwrap().kind else {
            panic!("terminal");
        };
        // This is the durable descriptor boundary written by ResumeHintChanged.
        session.resume_hint = Some("observed-provider-session-while-moving".into());
        app.persist();
        let pane = app
            .local_view_panes(&Scope::Channel(source))
            .into_iter()
            .find(|pane| pane.id == card)
            .unwrap();
        assert_eq!(pane.user_title.as_deref(), Some("Renamed while moving"));
        assert_ne!(pane.width_fraction, 0.5);
        pane
    }

    fn moving_terminal_kind() -> CardKind {
        let mut session = fresh_terminal_session(LaunchRecipe::shell("/repo".into()).unwrap());
        session.continuity = TerminalContinuity::Live {
            host_session_id: crate::model::HostSessionId::new("move-host"),
            boot_id: crate::model::BootId::new("move-boot"),
            host_incarnation: crate::model::HostIncarnation::new("move-incarnation"),
        };
        CardKind::Terminal(session)
    }

    fn assert_moving_metadata(app: &Cagents, expected: &Pane) {
        let card = app.card_state(expected.id).unwrap();
        assert_eq!(card.title, expected.title);
        assert_eq!(
            app.chrome_ui.user_titles.get(&expected.id),
            expected.user_title.as_ref()
        );
        assert_eq!(card.width_fraction, expected.width_fraction);
        assert_eq!(card.kind, expected.kind);
    }

    #[test]
    fn accepted_channel_move_retargets_user_and_observed_descriptor_edits_before_projection() {
        use super::super::workspace_model_sync::ModelUpdate;
        for watcher_first in [false, true] {
            let kind = moving_terminal_kind();
            let (mut app, source, destination, pane) = channel_move_fixture_with_kind(Some(kind));
            let (tx, rx) = async_channel::unbounded();
            app.workspace_model_tx = tx;
            drop(app.move_pane_to_channel(pane.id, destination));
            let ModelEdit::ChangeView {
                operation: transfer,
                ..
            } = rx.try_recv().unwrap()
            else {
                panic!("transfer");
            };
            let expected = edit_moving_terminal(&mut app, pane.id, source);
            let mut old = Vec::new();
            while let Ok(edit) = rx.try_recv() {
                if let ModelEdit::ChangeView {
                    scope,
                    operation,
                    change:
                        Change::Update {
                            before,
                            pane: after,
                        },
                    ..
                } = edit
                {
                    assert_eq!(scope, Scope::Channel(source));
                    old.push((
                        operation,
                        Change::Update {
                            before,
                            pane: after,
                        },
                    ));
                }
            }
            assert!(
                old.len() >= 3,
                "rename, resize and observed descriptor are separate real publications"
            );
            let mut model = app.workspace_model.clone();
            views::transfer(
                &mut model.views,
                &Scope::Channel(source),
                pane.id,
                &Scope::Channel(destination),
                None,
            )
            .unwrap();
            model.sequence += 1;
            model
                .views
                .iter_mut()
                .find(|view| view.scope == Scope::Channel(destination))
                .unwrap()
                .revision = 1;
            if watcher_first {
                drop(app.apply_workspace_model_update(ModelUpdate::Read(model.clone())));
                assert!(app.pane_transfer_pending(pane.id));
                assert_moving_metadata(&app, &expected);
            }
            drop(app.apply_workspace_model_update(ModelUpdate::ViewApplied {
                scope: Scope::Channel(source),
                operation: Some(transfer),
                model: model.clone(),
            }));
            assert_moving_metadata(&app, &expected);
            assert!(!app.pane_transfer_pending(pane.id));
            assert!(
                app.durable
                    .channels
                    .iter()
                    .find(|channel| channel.id == destination)
                    .unwrap()
                    .cards
                    .contains(&pane.id)
            );
            let mut retried = Vec::new();
            while let Ok(edit) = rx.try_recv() {
                if let ModelEdit::ChangeView {
                    scope,
                    expected_revision,
                    operation,
                    change:
                        Change::Update {
                            before,
                            pane: after,
                        },
                } = edit
                {
                    assert_eq!(scope, Scope::Channel(destination));
                    assert_eq!(expected_revision, 1);
                    assert!(!old.iter().any(|(previous, _)| *previous == operation));
                    retried.push((
                        operation,
                        Change::Update {
                            before,
                            pane: after,
                        },
                    ));
                }
            }
            assert_eq!(
                retried.len(),
                old.len(),
                "watcher and transfer acknowledgment must not duplicate retries"
            );
            for (operation, change) in old {
                assert!(
                    !model
                        .views
                        .iter_mut()
                        .find(|view| view.scope == Scope::Channel(source))
                        .unwrap()
                        .apply(&change)
                        .unwrap()
                );
                model.sequence += 1;
                drop(app.apply_workspace_model_update(ModelUpdate::ViewApplied {
                    scope: Scope::Channel(source),
                    operation: Some(operation),
                    model: model.clone(),
                }));
                assert_moving_metadata(&app, &expected);
                assert!(
                    !app.shared_views.pending.is_empty(),
                    "an old-scope acknowledgment cannot retire its retargeted intent"
                );
            }
            for (operation, change) in retried {
                model
                    .views
                    .iter_mut()
                    .find(|view| view.scope == Scope::Channel(destination))
                    .unwrap()
                    .apply(&change)
                    .unwrap();
                model.sequence += 1;
                drop(app.apply_workspace_model_update(ModelUpdate::ViewApplied {
                    scope: Scope::Channel(destination),
                    operation: Some(operation),
                    model: model.clone(),
                }));
                assert_moving_metadata(&app, &expected);
            }
            assert!(app.shared_views.pending.is_empty());
            assert!(
                rx.try_recv().is_err(),
                "settled updates do not publish a feedback loop"
            );
        }
    }

    #[test]
    fn refused_channel_move_keeps_metadata_edits_in_the_original_scope() {
        use super::super::workspace_model_sync::ModelUpdate;
        let kind = moving_terminal_kind();
        let (mut app, source, destination, pane) = channel_move_fixture_with_kind(Some(kind));
        let (tx, rx) = async_channel::unbounded();
        app.workspace_model_tx = tx;
        drop(app.move_pane_to_channel(pane.id, destination));
        let ModelEdit::ChangeView {
            operation: transfer,
            ..
        } = rx.try_recv().unwrap()
        else {
            panic!("transfer");
        };
        let expected = edit_moving_terminal(&mut app, pane.id, source);
        let pending = app.shared_views.pending.clone();
        assert!(pending.len() >= 3);
        while rx.try_recv().is_ok() {}
        drop(
            app.apply_workspace_model_update(ModelUpdate::PaneTransferFailed {
                operation: transfer,
                error: "destination removed".into(),
            }),
        );
        assert_moving_metadata(&app, &expected);
        assert_eq!(app.shared_views.pending, pending);
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
            rx.try_recv().is_err(),
            "a refused transfer must not retarget or duplicate metadata operations"
        );
    }

    #[test]
    fn channel_move_keeps_current_descriptor_and_independent_user_edits() {
        use super::super::workspace_model_sync::ModelUpdate;
        for replacement in [
            "session",
            "proof",
            "unproved-before",
            "unproved-current",
            "incomplete-both",
            "same-process-metadata",
            "other-current-field",
        ] {
            let mut kind = moving_terminal_kind();
            let CardKind::Terminal(session) = &mut kind else {
                panic!("terminal");
            };
            if replacement == "unproved-before" {
                session.continuity = TerminalContinuity::RelaunchAvailable {
                    recipe: session.launch.clone(),
                    last_exit: None,
                };
            } else if replacement == "incomplete-both" {
                let TerminalContinuity::Live { boot_id, .. } = &mut session.continuity else {
                    panic!("proof");
                };
                *boot_id = crate::model::BootId::new("");
            }
            let (mut app, source, destination, pane) = channel_move_fixture_with_kind(Some(kind));
            let (tx, rx) = async_channel::unbounded();
            app.workspace_model_tx = tx;
            drop(app.move_pane_to_channel(pane.id, destination));
            let ModelEdit::ChangeView {
                operation: transfer,
                ..
            } = rx.try_recv().unwrap()
            else {
                panic!("transfer");
            };
            let mut expected = edit_moving_terminal(&mut app, pane.id, source);
            while rx.try_recv().is_ok() {}
            let mut model = app.workspace_model.clone();
            views::transfer(
                &mut model.views,
                &Scope::Channel(source),
                pane.id,
                &Scope::Channel(destination),
                None,
            )
            .unwrap();
            let destination_view = model
                .views
                .iter_mut()
                .find(|view| view.scope == Scope::Channel(destination))
                .unwrap();
            let current = &mut destination_view.panes[0].kind;
            let CardKind::Terminal(session) = current else {
                panic!("terminal");
            };
            match replacement {
                "session" => session.id = TerminalId::new(),
                "proof" => {
                    let TerminalContinuity::Live {
                        host_incarnation, ..
                    } = &mut session.continuity
                    else {
                        panic!("proof");
                    };
                    *host_incarnation =
                        crate::model::HostIncarnation::new("replacement-incarnation");
                }
                "unproved-current" => {
                    session.continuity = TerminalContinuity::RelaunchAvailable {
                        recipe: session.launch.clone(),
                        last_exit: None,
                    }
                }
                _ => {}
            }
            if replacement == "other-current-field" {
                session.transcript_path = Some("/authoritative-current/transcript".into());
            } else {
                session.resume_hint = Some("authoritative-current-session".into());
            }
            expected.kind = current.clone();
            model.sequence += 1;
            drop(
                app.update(Message::WorkspaceModelObserved(ModelUpdate::ViewApplied {
                    scope: Scope::Channel(source),
                    operation: Some(transfer),
                    model: model.clone(),
                })),
            );
            assert_moving_metadata(&app, &expected);
            assert!(
                app.notices.recent().any(|entry| entry
                    .notice
                    .message
                    .contains("incoming session update could not be verified")),
                "{replacement}"
            );
            let mut retried = Vec::new();
            while let Ok(edit) = rx.try_recv() {
                if let ModelEdit::ChangeView {
                    scope,
                    operation,
                    change:
                        Change::Update {
                            before,
                            pane: after,
                        },
                    ..
                } = edit
                {
                    assert_eq!(scope, Scope::Channel(destination));
                    assert_eq!(
                        before.kind, after.kind,
                        "an old actor must not overwrite its replacement: {replacement}"
                    );
                    retried.push((
                        operation,
                        Change::Update {
                            before,
                            pane: after,
                        },
                    ));
                }
            }
            assert!(
                !retried.is_empty(),
                "title and width intentions still need acknowledgment"
            );
            for (operation, change) in retried {
                model
                    .views
                    .iter_mut()
                    .find(|view| view.scope == Scope::Channel(destination))
                    .unwrap()
                    .apply(&change)
                    .unwrap();
                model.sequence += 1;
                drop(app.apply_workspace_model_update(ModelUpdate::ViewApplied {
                    scope: Scope::Channel(destination),
                    operation: Some(operation),
                    model: model.clone(),
                }));
                assert_moving_metadata(&app, &expected);
            }
            assert!(app.shared_views.pending.is_empty());
        }
    }

    #[test]
    fn channel_move_refuses_stale_start_and_scope_fields_without_a_kind_delta() {
        use super::super::workspace_model_sync::ModelUpdate;
        let (mut app, source, destination, pane) =
            channel_move_fixture_with_kind(Some(moving_terminal_kind()));
        let (tx, rx) = async_channel::unbounded();
        app.workspace_model_tx = tx;
        drop(app.move_pane_to_channel(pane.id, destination));
        let ModelEdit::ChangeView {
            operation: transfer,
            ..
        } = rx.try_recv().unwrap()
        else {
            panic!("transfer");
        };
        // Exercise the pending Update boundary independently of whether a
        // particular producer currently emits both source-sensitive deltas.
        let mut before = pane.clone();
        before.pending_terminal_start = true;
        before.scope = Some("/previous-scope".into());
        let mut desired = before.clone();
        desired.pending_terminal_start = false;
        desired.scope = Some("/stale-scope".into());
        desired.title = "Preserved presentation edit".into();
        app.shared_views.pending.push((
            uuid::Uuid::new_v4(),
            Scope::Channel(source),
            Change::Update {
                before,
                pane: desired.clone(),
            },
        ));
        let (operation, scope, change) = app.shared_views.pending.last().unwrap().clone();
        app.publish_model_edit(ModelEdit::ChangeView {
            scope,
            expected_revision: 17,
            operation,
            change,
        });
        while rx.try_recv().is_ok() {}
        let mut model = app.workspace_model.clone();
        // Metadata adoption must not publish the old pane before projection.
        model
            .channels
            .iter_mut()
            .find(|channel| channel.id == source)
            .unwrap()
            .title = "Renamed source while moving".into();
        views::transfer(
            &mut model.views,
            &Scope::Channel(source),
            pane.id,
            &Scope::Channel(destination),
            None,
        )
        .unwrap();
        let current = &mut model
            .views
            .iter_mut()
            .find(|view| view.scope == Scope::Channel(destination))
            .unwrap()
            .panes[0];
        let CardKind::Terminal(session) = &mut current.kind else {
            panic!("terminal");
        };
        session.id = TerminalId::new();
        current.pending_terminal_start = true;
        current.scope = Some("/authoritative-scope".into());
        let expected = current.clone();
        model.sequence += 1;
        drop(app.apply_workspace_model_update(ModelUpdate::ViewApplied {
            scope: Scope::Channel(source),
            operation: Some(transfer),
            model: model.clone(),
        }));
        let ModelEdit::ChangeView { scope, change, .. } = rx.try_recv().unwrap() else {
            panic!("presentation retry");
        };
        assert_eq!(scope, Scope::Channel(destination));
        assert_eq!(
            app.durable
                .channels
                .iter()
                .find(|channel| channel.id == source)
                .unwrap()
                .title,
            "Renamed source while moving"
        );
        let Change::Update {
            before,
            pane: after,
        } = &change
        else {
            panic!("update");
        };
        assert_eq!(before.kind, after.kind);
        assert_eq!(before.pending_terminal_start, after.pending_terminal_start);
        assert_eq!(before.scope, after.scope);
        let destination = model
            .views
            .iter_mut()
            .find(|view| view.scope == Scope::Channel(destination))
            .unwrap();
        destination.apply(&change).unwrap();
        assert_eq!(destination.panes[0].title, desired.title);
        assert_eq!(destination.panes[0].kind, expected.kind);
        assert_eq!(
            destination.panes[0].pending_terminal_start,
            expected.pending_terminal_start
        );
        assert_eq!(destination.panes[0].scope, expected.scope);
        assert!(app.notices.recent().any(|entry| {
            entry
                .notice
                .message
                .contains("incoming session update could not be verified")
        }));
        let unexpected = rx.try_recv();
        assert!(
            unexpected.is_err(),
            "unexpected publication: {unexpected:?}"
        );
    }

    #[test]
    fn transferred_updates_cannot_acquire_missing_or_foreign_source_authority() {
        use super::super::workspace_model_sync::ModelUpdate;
        for control in ["uncaptured", "foreign-watch-first", "foreign-registry"] {
            let (mut app, source, destination, pane) = channel_move_fixture();
            let (tx, rx) = async_channel::unbounded();
            app.workspace_model_tx = tx;
            drop(app.move_pane_to_channel(pane.id, destination));
            let ModelEdit::ChangeView {
                operation: transfer,
                ..
            } = rx.try_recv().unwrap()
            else {
                panic!("transfer");
            };
            let operation = uuid::Uuid::new_v4();
            let mut desired = pane.clone();
            desired.title = "Unverified earlier title".into();
            let change = Change::Update {
                before: pane.clone(),
                pane: desired,
            };
            app.shared_views
                .pending
                .push((operation, Scope::Channel(source), change.clone()));
            if control != "uncaptured" {
                app.publish_model_edit(ModelEdit::ChangeView {
                    scope: Scope::Channel(source),
                    expected_revision: 17,
                    operation,
                    change,
                });
                while rx.try_recv().is_ok() {}
            }
            let mut model = app.workspace_model.clone();
            views::transfer(
                &mut model.views,
                &Scope::Channel(source),
                pane.id,
                &Scope::Channel(destination),
                None,
            )
            .unwrap();
            if control == "foreign-watch-first" {
                for channel in &mut model.channels {
                    channel.repository = "/foreign/.git".into();
                }
            } else if control == "foreign-registry" {
                app.shared_views.source_bases.get_mut(&operation).unwrap().0 = uuid::Uuid::new_v4();
            }
            model.sequence += 1;
            app.workspace_model = model.clone();
            drop(app.apply_workspace_model_update(ModelUpdate::ViewApplied {
                scope: Scope::Channel(source),
                operation: Some(transfer),
                model,
            }));
            assert!(
                rx.try_recv().is_err(),
                "an unknown source must not gain a fresh receipt: {control}"
            );
            assert!(
                !app.shared_views
                    .pending
                    .iter()
                    .any(|(id, _, _)| *id == operation)
            );
            assert!(!app.shared_views.source_bases.contains_key(&operation));
            assert_eq!(app.card_state(pane.id).unwrap().title, pane.title);
            assert!(app.notices.recent().any(|entry| {
                entry
                    .notice
                    .message
                    .contains("incoming session update could not be verified")
            }));
        }
    }

    /// Another window made the same session update, but this one also renamed
    /// the pane. Quiet is only right while the name is still on its way.
    #[test]
    fn losing_a_session_update_race_stays_quiet_only_if_its_rename_survives() {
        use super::super::workspace_model_sync::ModelUpdate;
        for can_retry in [true, false] {
            let (mut app, source, _, before) = channel_move_fixture();
            let (tx, rx) = async_channel::unbounded();
            app.workspace_model_tx = tx;
            let operation = uuid::Uuid::new_v4();
            let mut desired = before.clone();
            desired.title = "Renamed here".into();
            desired.scope = Some("/same-new-source".into());
            let change = Change::Update {
                before: before.clone(),
                pane: desired.clone(),
            };
            app.shared_views
                .pending
                .push((operation, Scope::Channel(source), change.clone()));
            if can_retry {
                app.publish_model_edit(ModelEdit::ChangeView {
                    scope: Scope::Channel(source),
                    expected_revision: 17,
                    operation,
                    change: change.clone(),
                });
                rx.try_recv().unwrap();
            }
            let mut model = app.workspace_model.clone();
            model.views[0].panes[0].scope = desired.scope.clone();
            model.sequence += 1;
            drop(
                app.apply_workspace_model_update(ModelUpdate::ViewConflicted {
                    scope: Scope::Channel(source),
                    operation,
                    change,
                    model,
                    error: "The pane source changed".into(),
                }),
            );

            let warned = app
                .notices
                .recent()
                .any(|entry| entry.notice.message.contains("session update was refused"));
            assert_eq!(warned, !can_retry);
            if can_retry {
                let ModelEdit::ChangeView { change, .. } = rx.try_recv().unwrap() else {
                    panic!("presentation retry");
                };
                let Change::Update { pane, .. } = change else {
                    panic!("update");
                };
                assert_eq!(pane.title, desired.title);
            } else {
                assert!(rx.try_recv().is_err());
            }
        }
    }

    #[test]
    fn a_source_conflict_refuses_session_intent_and_reissues_only_safe_presentation() {
        use super::super::workspace_model_sync::ModelUpdate;
        for control in [
            "current",
            "missing",
            "foreign-project",
            "foreign-watch-first",
            "uncaptured",
            "later-title",
        ] {
            let (mut app, source, _, before) = channel_move_fixture();
            let (tx, rx) = async_channel::unbounded();
            app.workspace_model_tx = tx;
            let operation = uuid::Uuid::new_v4();
            let mut desired = before.clone();
            desired.title = "Earlier captured title".into();
            desired.user_title = Some("User title".into());
            desired.width_fraction = 0.8;
            desired.pending_terminal_start = true;
            desired.scope = Some("/stale-source".into());
            let change = Change::Update {
                before: before.clone(),
                pane: desired.clone(),
            };
            app.shared_views
                .pending
                .push((operation, Scope::Channel(source), change.clone()));
            if control != "uncaptured" {
                app.publish_model_edit(ModelEdit::ChangeView {
                    scope: Scope::Channel(source),
                    expected_revision: 17,
                    operation,
                    change: change.clone(),
                });
                assert!(matches!(rx.try_recv().unwrap(), ModelEdit::ChangeView {
                    operation: queued, .. } if queued == operation));
            }
            let later_operation = uuid::Uuid::new_v4();
            if control == "later-title" {
                let mut later = before.clone();
                later.title = "Later user title".into();
                app.shared_views.pending.push((
                    later_operation,
                    Scope::Channel(source),
                    Change::Update {
                        before: before.clone(),
                        pane: later,
                    },
                ));
            }
            let mut model = app.workspace_model.clone();
            let current = &mut model.views[0].panes[0];
            let CardKind::Browser(browser) = &mut current.kind else {
                panic!("browser");
            };
            browser.current_url = "https://authoritative.example".parse().unwrap();
            current.scope = Some("/authoritative-source".into());
            let expected = current.clone();
            if control == "missing" {
                model.views[0].panes.clear();
            }
            if matches!(control, "foreign-project" | "foreign-watch-first") {
                model
                    .channels
                    .iter_mut()
                    .find(|channel| channel.id == source)
                    .unwrap()
                    .repository = "/foreign/.git".into();
            }
            model.sequence += 1;
            if control == "foreign-watch-first" {
                // A preceding watcher must not redefine this operation's
                // captured logical source before its conflict reply arrives.
                app.workspace_model = model.clone();
            }
            drop(
                app.apply_workspace_model_update(ModelUpdate::ViewConflicted {
                    scope: Scope::Channel(source),
                    operation,
                    change,
                    model: model.clone(),
                    error: "The pane source changed".into(),
                }),
            );
            assert!(
                !app.shared_views
                    .pending
                    .iter()
                    .any(|(id, _, _)| *id == operation)
            );
            assert!(
                app.notices
                    .recent()
                    .any(|entry| entry.notice.message.contains("session update was refused"))
            );
            if matches!(
                control,
                "missing" | "foreign-project" | "foreign-watch-first" | "uncaptured"
            ) {
                assert!(
                    rx.try_recv().is_err(),
                    "refusal cannot rename a missing or foreign source"
                );
                assert!(!app.shared_views.source_bases.contains_key(&operation));
                continue;
            }
            let ModelEdit::ChangeView {
                operation: retry,
                change,
                ..
            } = rx.try_recv().unwrap()
            else {
                panic!("presentation retry");
            };
            assert_ne!(retry, operation);
            let Change::Update { before, pane } = &change else {
                panic!("update");
            };
            assert_eq!(before.kind, expected.kind);
            assert_eq!(pane.kind, expected.kind);
            assert_eq!(before.pending_terminal_start, pane.pending_terminal_start);
            assert_eq!(before.scope, pane.scope);
            assert_eq!(pane.user_title, desired.user_title);
            assert_eq!(pane.width_fraction, desired.width_fraction);
            let expected_title = if control == "later-title" {
                &expected.title
            } else {
                &desired.title
            };
            assert_eq!(&pane.title, expected_title);
            // A delayed old receipt cannot retire the independent new intent.
            drop(app.apply_workspace_model_update(ModelUpdate::ViewApplied {
                scope: Scope::Channel(source),
                operation: Some(operation),
                model: model.clone(),
            }));
            assert!(
                app.shared_views
                    .pending
                    .iter()
                    .any(|(id, _, _)| *id == retry)
            );
            if control == "later-title" {
                assert!(
                    app.shared_views
                        .pending
                        .iter()
                        .any(|(id, _, _)| *id == later_operation)
                );
                assert_eq!(app.card_state(pane.id).unwrap().title, "Later user title");
            }
            model.views[0].apply(&change).unwrap();
            model.sequence += 1;
            drop(app.apply_workspace_model_update(ModelUpdate::ViewApplied {
                scope: Scope::Channel(source),
                operation: Some(retry),
                model,
            }));
            assert!(
                !app.shared_views
                    .pending
                    .iter()
                    .any(|(id, _, _)| *id == retry)
            );
            assert!(!app.shared_views.source_bases.contains_key(&retry));
            assert!(rx.try_recv().is_err());
        }
    }

    #[test]
    fn channel_move_does_not_retarget_another_panes_pending_update() {
        use super::super::workspace_model_sync::ModelUpdate;
        let (mut app, source, destination, pane) = channel_move_fixture();
        let mut other = pane.clone();
        other.id = CardId::new();
        let CardKind::Browser(session) = &mut other.kind else {
            panic!("browser");
        };
        session.id = BrowserId::new();
        app.workspace_model
            .views
            .iter_mut()
            .find(|view| view.scope == Scope::Channel(source))
            .unwrap()
            .panes
            .push(other.clone());
        drop(app.reconcile_shared_views());
        let (tx, rx) = async_channel::unbounded();
        app.workspace_model_tx = tx;
        drop(app.move_pane_to_channel(pane.id, destination));
        let ModelEdit::ChangeView {
            operation: transfer,
            ..
        } = rx.try_recv().unwrap()
        else {
            panic!("transfer");
        };
        let unrelated = uuid::Uuid::new_v4();
        let mut renamed = other.clone();
        renamed.title = "Keep another pane's edit".into();
        let intent = (
            unrelated,
            Scope::Channel(source),
            Change::Update {
                before: other.clone(),
                pane: renamed,
            },
        );
        app.shared_views.pending.push(intent.clone());
        let mut model = app.workspace_model.clone();
        views::transfer(
            &mut model.views,
            &Scope::Channel(source),
            pane.id,
            &Scope::Channel(destination),
            None,
        )
        .unwrap();
        model.sequence += 1;
        drop(app.apply_workspace_model_update(ModelUpdate::ViewApplied {
            scope: Scope::Channel(source),
            operation: Some(transfer),
            model,
        }));
        assert_eq!(app.shared_views.pending, vec![intent]);
        assert_eq!(
            app.card_state(other.id).unwrap().title,
            "Keep another pane's edit"
        );
        assert!(
            rx.try_recv().is_err(),
            "unrelated intentions retain their existing operation receipt"
        );
    }

    #[test]
    fn active_repository_save_blocks_channel_commands_picker_and_direct_moves() {
        let (mut app, source, destination, pane) = channel_move_fixture();
        let (tx, rx) = async_channel::unbounded();
        app.workspace_model_tx = tx;
        assert!(app.pane_has_move_destination(pane.id));
        let (runtime, _) = RepositoryRuntime::post_persist_failure_fixture(pane.id);
        let kind = CardKind::Repository(runtime.descriptor.clone());
        app.card_state_mut(pane.id).unwrap().kind = kind.clone();
        app.workspace_model
            .views
            .iter_mut()
            .flat_map(|view| &mut view.panes)
            .find(|candidate| candidate.id == pane.id)
            .unwrap()
            .kind = kind;
        app.cards
            .insert(pane.id, CardRuntime::Repository(Box::new(runtime)));
        assert!(app.focused_repository_is_saving());
        assert!(!app.pane_has_move_destination(pane.id));
        assert!(app.pane_move_destinations(pane.id).is_empty());
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
    fn asynchronous_terminal_close_blocks_moves_until_its_actual_cancel() {
        use super::super::terminal_close::CloseOutcome;
        let (mut app, source, destination, pane) =
            channel_move_fixture_with_kind(Some(moving_terminal_kind()));
        let (tx, rx) = async_channel::unbounded();
        app.workspace_model_tx = tx;
        assert!(app.can_move_pane_to_channel(pane.id, destination));
        // Starting the real application close reserves its asynchronous owner,
        // while the pane and runtime remain open and no dialog is visible yet.
        drop(app.begin_card_close(pane.id));
        assert!(app.terminal_close_pending(pane.id));
        assert!(app.card_state(pane.id).unwrap().existence.is_open());
        assert!(!app.modal_is_open());
        assert!(app.pane_move_destinations(pane.id).is_empty());
        drop(app.update(Message::MovePaneToChannel {
            card: pane.id,
            channel: destination,
        }));
        drop(app.update(Message::PaneDropToChannel {
            generation: app.workspace_view_generation,
            card: pane.id,
            channel: destination,
        }));
        assert!(
            rx.try_recv().is_err(),
            "close cannot be overtaken by a transfer"
        );
        assert!(!app.pane_transfer_pending(pane.id));
        let intent = app.terminal_close.as_ref().unwrap();
        let (terminal, proof, generation) =
            (intent.terminal, intent.proof.clone(), intent.generation);
        drop(app.terminal_close_finished(
            pane.id,
            terminal,
            proof,
            generation,
            CloseOutcome::Active,
        ));
        assert!(app.modal_is_open());
        assert!(app.pane_move_destinations(pane.id).is_empty());
        drop(app.cancel_terminal_close());
        assert!(app.can_move_pane_to_channel(pane.id, destination));
        drop(app.move_pane_to_channel(pane.id, destination));
        assert!(matches!(rx.try_recv().unwrap(), ModelEdit::ChangeView {
            scope: Scope::Channel(from), change: Change::Transfer { id, destination: Scope::Channel(to), .. }, ..
        } if from == source && id == pane.id && to == destination));
    }

    #[test]
    fn focus_handoff_close_keeps_its_source_until_commit() {
        let (mut app, _, destination, pane) = channel_move_fixture();
        let (tx, rx) = async_channel::unbounded();
        app.workspace_model_tx = tx;
        drop(app.request_card_removal(pane.id));
        assert_eq!(app.workspace.pending_close().unwrap().unmounting, pane.id);
        assert!(app.card_state(pane.id).unwrap().existence.is_open());
        assert!(app.pane_move_destinations(pane.id).is_empty());
        drop(app.move_pane_to_channel(pane.id, destination));
        assert!(rx.try_recv().is_err());
        assert!(!app.pane_transfer_pending(pane.id));
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
    fn move_palette_keeps_selected_destination_across_live_ranking_changes() {
        for query in [false, true] {
            let (mut app, source, destination, pane) = channel_move_fixture();
            app.workspace_model
                .channels
                .iter_mut()
                .find(|channel| channel.id == destination)
                .unwrap()
                .title = "Other needle destination".into();
            let (tx, rx) = async_channel::unbounded();
            app.workspace_model_tx = tx;
            drop(app.update_palette(super::super::palette_ui::Action::MovePane(pane.id)));
            if query {
                drop(app.update_palette(super::super::palette_ui::Action::Query("needle".into())));
            }
            let mut stronger = app.workspace_model.channel(destination).unwrap().clone();
            stronger.id = ChannelId::new();
            stronger.title = "needle first".into();
            stronger.ordinal += 1;
            app.workspace_model.channels.push(stronger);
            drop(app.update_palette(super::super::palette_ui::Action::Submit));
            assert!(matches!(rx.try_recv().unwrap(), ModelEdit::ChangeView {
                scope: Scope::Channel(from), change: Change::Transfer { id, destination: Scope::Channel(to), .. }, ..
            } if from == source && id == pane.id && to == destination));
            assert!(app.pane_transfer_pending(pane.id));
        }
    }

    #[test]
    fn move_palette_does_not_substitute_another_live_destination_when_selected_one_disappears() {
        let (mut app, _, destination, pane) = channel_move_fixture();
        let (tx, rx) = async_channel::unbounded();
        app.workspace_model_tx = tx;
        drop(app.update_palette(super::super::palette_ui::Action::MovePane(pane.id)));
        let mut replacement = app.workspace_model.channel(destination).unwrap().clone();
        replacement.id = ChannelId::new();
        app.workspace_model
            .channels
            .retain(|channel| channel.id != destination);
        app.workspace_model.channels.push(replacement);
        drop(app.update_palette(super::super::palette_ui::Action::Submit));
        assert!(rx.try_recv().is_err());
        assert!(!app.pane_transfer_pending(pane.id));
        assert!(app.palette.is_open());
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
    fn repository_scope_closure_keeps_all_owners_of_a_shared_path() {
        let (mut app, source, _, pane) = channel_move_fixture();
        let first = PathBuf::from("/repo/.git");
        let second = PathBuf::from("/other/.git");
        let path = PathBuf::from("/repo/shared-tree");
        let first_project = app.durable.channels[0].project_id;
        for (id, repository) in [
            (first_project, first.clone()),
            (ProjectId::new(), second.clone()),
        ] {
            app.sidebar_model.projects.push(SidebarProject::new(
                id,
                RepositoryId::new(),
                "repo".into(),
                repository,
                vec![],
                vec![],
            ));
        }
        let a = Scope::Worktree {
            repository: first,
            path: path.clone(),
        };
        let b = Scope::Worktree {
            repository: second.clone(),
            path: path.clone(),
        };
        // Both scopes receive this path's members in local_views. Put `a`
        // last to catch a path index that keeps only the last authority.
        app.workspace_model
            .views
            .extend([View::empty(b.clone()), View::empty(a.clone())]);
        app.known_worktree_roots.insert(path.clone());
        let durable = Arc::make_mut(&mut app.durable);
        for channel in &mut durable.channels {
            channel.cards.retain(|id| *id != pane.id);
        }
        durable.card_scopes.insert(pane.id, path);
        let selected = app.repository_view_scopes(&second, &HashSet::new());
        assert!(selected.contains(&a));
        assert!(selected.contains(&b));
        assert!(selected.contains(&Scope::Channel(source)));
    }

    #[test]
    fn repository_scope_closure_follows_adopted_scopes_and_pending_transfers() {
        let (mut app, source, destination, pane) = channel_move_fixture();
        app.shared_views.pending.push((
            uuid::Uuid::new_v4(),
            Scope::Channel(source),
            Change::Transfer {
                id: pane.id,
                destination: Scope::Channel(destination),
                before: None,
            },
        ));
        let selected = app.repository_view_scopes(
            Path::new("/unrelated"),
            &HashSet::from([Scope::Channel(destination)]),
        );
        assert!(selected.contains(&Scope::Channel(source)));
        assert!(selected.contains(&Scope::Channel(destination)));
    }

    #[test]
    fn repository_loads_project_only_their_affected_views() {
        let (mut app, unrelated, _, pane) = channel_move_fixture();
        let baseline = app.shared_views.baseline[&Scope::Channel(unrelated)].clone();
        let pending = (
            uuid::Uuid::new_v4(),
            Scope::Channel(unrelated),
            Change::Remove { id: CardId::new() },
        );
        app.shared_views.pending.push(pending.clone());
        let mut loaded = Vec::new();
        for index in 0..6 {
            let id = ProjectId::new();
            let repository = PathBuf::from(format!("/repositories/project-{index}"));
            app.sidebar_model.projects.push(SidebarProject::new(
                id,
                RepositoryId::new(),
                format!("project-{index}"),
                repository.clone(),
                vec![],
                vec![],
            ));
            app.repository_generations.insert(id, 1);
            app.workspace_model
                .projects
                .push(crate::workspace_model::ModelProject {
                    repository: repository.clone(),
                    name: format!("project-{index}"),
                    kind: Default::default(),
                    ordinal: index,
                });
            for view in 0..8 {
                let scope = Scope::Worktree {
                    repository: repository.clone(),
                    path: repository.join(format!("tree-{view}")),
                };
                app.shared_views.initialized.insert(scope.clone());
                app.workspace_model.views.push(View::empty(scope));
            }
            loaded.push(LoadedRepository {
                project: ProjectState {
                    kind: Default::default(),
                    id,
                    generation: 0,
                    name: format!("project-{index}"),
                    repository,
                    existence: Existence::Open,
                },
                repository: None,
                worktrees: vec![],
                github: GitHubAvailability::NotAssociated,
                review_repository: None,
                head_repositories: HashMap::new(),
            });
        }
        app.shared_views.view_projection_count.set(0);
        for loaded in loaded {
            drop(app.update(Message::RepositoryLoaded {
                open_cards: false,
                project_id: loaded.project.id,
                generation: 1,
                result: Ok(Box::new(loaded)),
            }));
        }
        assert_eq!(
            app.shared_views.view_projection_count.get(),
            6 * 8,
            "loading each repository must not project every other repository's views"
        );
        assert!(
            app.durable
                .channels
                .iter()
                .any(|channel| channel.id == unrelated && channel.cards.contains(&pane.id))
        );
        assert!(app.card_state(pane.id).unwrap().existence.is_open());
        assert_eq!(
            app.shared_views.baseline[&Scope::Channel(unrelated)],
            baseline
        );
        assert!(
            app.shared_views.pending.contains(&pending),
            "an unrelated operation keeps its receipt and scope"
        );
    }

    fn desktop_inventory_fixture() -> (Cagents, crate::workspace_model::WorkspaceModel) {
        let (mut app, _, _, template) = fixture();
        let repository = PathBuf::from("/repo/.git");
        let project = app.durable.channels[0].project_id;
        app.sidebar_model.projects.push(SidebarProject::new(
            project,
            RepositoryId::new(),
            "repo".into(),
            repository.clone(),
            vec![],
            vec![],
        ));
        let mut model = crate::workspace_model::WorkspaceModel {
            identity: app.workspace_model.identity,
            sequence: 17,
            ..Default::default()
        };
        let durable = Arc::make_mut(&mut app.durable);
        durable.channels.clear();
        durable.cards.clear();
        durable.card_scopes.clear();
        durable.active_channel = None;
        for index in 0..30 {
            let id = ChannelId::new();
            durable.channels.push(ProjectChannel {
                id,
                project_id: project,
                title: format!("Channel {index}"),
                title_source: Default::default(),
                ordinal: index,
                shared: true,
                cards: vec![],
                focused: None,
            });
            model.channels.push(crate::workspace_model::ModelChannel {
                id,
                repository: repository.clone(),
                title: format!("Channel {index}"),
                title_source: Default::default(),
                ordinal: index,
            });
            let scope = Scope::Channel(id);
            model.views.push(View::empty(scope.clone()));
            app.shared_views.initialized.insert(scope);
        }
        // The desktop has many historical cards and scopes, not 555 live
        // providers. Only these 122 panes are open shared descriptors.
        for index in 0..555 {
            let mut pane = template.clone();
            pane.id = CardId::new();
            pane.title = format!("Pane {index}");
            pane.scope = Some(PathBuf::from(format!("/repo/scope-{index}")));
            if let CardKind::Browser(browser) = &mut pane.kind {
                browser.id = BrowserId::new();
            }
            let open = index < 122;
            durable.cards.push(CardState {
                id: pane.id,
                generation: 0,
                title: pane.title.clone(),
                width_fraction: pane.width_fraction,
                existence: if open {
                    Existence::Open
                } else {
                    super::super::project_ui::removed_existence("historical fixture")
                },
                kind: pane.kind.clone(),
            });
            durable
                .card_scopes
                .insert(pane.id, pane.scope.clone().unwrap());
            app.cards.insert(pane.id, CardRuntime::Completed);
            if open {
                let channel = index % 30;
                durable.channels[channel].cards.push(pane.id);
                durable.channels[channel].focused.get_or_insert(pane.id);
                model.views[channel].panes.push(pane);
            }
        }
        for index in 0..1218 {
            let scope = Scope::Worktree {
                repository: repository.clone(),
                path: format!("/repo/old-{index}").into(),
            };
            app.shared_views.initialized.insert(scope.clone());
            model.views.push(View::empty(scope));
        }
        assert_eq!(model.views.len(), 1248);
        assert_eq!(
            model
                .views
                .iter()
                .map(|view| view.panes.len())
                .sum::<usize>(),
            122
        );
        (app, model)
    }

    #[test]
    fn complete_model_echo_reuses_one_projection_at_desktop_inventory_scale() {
        use super::super::workspace_model_sync::ModelUpdate;
        let (mut app, mut model) = desktop_inventory_fixture();
        let (tx, rx) = async_channel::unbounded();
        app.workspace_model_tx = tx;
        let before = app.durable.clone();
        for _ in 0..8 {
            app.shared_views.local_projection_count.set(0);
            drop(app.apply_workspace_model_update(ModelUpdate::Read(model.clone())));
            assert_eq!(
                app.shared_views.local_projection_count.get(),
                1,
                "the completed baseline and publication share one real LocalViews pass"
            );
            assert_eq!(app.durable.cards, before.cards);
            assert_eq!(app.durable.channels, before.channels);
            assert_eq!(app.durable.card_scopes, before.card_scopes);
            assert_eq!(app.workspace.focused(), None);
            assert!(
                !std::iter::from_fn(|| rx.try_recv().ok()).any(|edit| matches!(
                    edit,
                    ModelEdit::ChangeView { .. } | ModelEdit::SeedView { .. }
                ))
            );
        }
        // This is not an equal-model drop: actual canonical changes still
        // project whole pane fields, and a later equal Read rolls optimism back.
        let id = model.views[0].panes[0].id;
        model.sequence += 1;
        model.views[0].revision += 1;
        model.views[0].panes[0].title = "Remote title".into();
        model.views[0].panes[0].width_fraction = 0.72;
        drop(app.apply_workspace_model_update(ModelUpdate::Read(model.clone())));
        assert_eq!(app.card_state(id).unwrap().title, "Remote title");
        assert_eq!(app.card_state(id).unwrap().width_fraction, 0.72);
        app.card_state_mut(id).unwrap().title = "Unaccepted optimism".into();
        drop(app.apply_workspace_model_update(ModelUpdate::Read(model)));
        assert_eq!(app.card_state(id).unwrap().title, "Remote title");
        assert_eq!(app.durable.cards.len(), 555);
    }

    /// Process-fresh synchronous reducer cost; excludes task execution, providers
    /// and compositor presentation. Run each revision with --profile dev-runtime.
    #[test]
    #[ignore = "manual paired fw16 performance measurement"]
    fn measure_desktop_startup_reconciliation() {
        use super::super::workspace_model_sync::ModelUpdate;
        fn cpu_ns() -> u64 {
            let mut value = libc::timespec { tv_sec: 0, tv_nsec: 0 };
            assert_eq!(unsafe {
                libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut value)
            }, 0);
            value.tv_sec as u64 * 1_000_000_000 + value.tv_nsec as u64
        }
        let (mut app, model) = desktop_inventory_fixture();
        let (tx, _rx) = async_channel::unbounded();
        app.workspace_model_tx = tx;
        let wall = Instant::now();
        let cpu = cpu_ns();
        for _ in 0..8 {
            drop(app.apply_workspace_model_update(ModelUpdate::Read(model.clone())));
        }
        println!("desktop_reconciliation views={} cards={} panes={} reads=8 cpu_ns={} wall_ns={}",
            model.views.len(), app.durable.cards.len(),
            model.views.iter().map(|view| view.panes.len()).sum::<usize>(),
            cpu_ns() - cpu, wall.elapsed().as_nanos());
        assert_eq!(app.durable.cards.len(), 555);
        assert_eq!(app.workspace_model.views, model.views);
    }

    #[test]
    fn an_exact_populated_view_does_not_need_a_migration_receipt() {
        let (mut app, source, _, pane) = channel_move_fixture();
        let scope = Scope::Channel(source);
        app.shared_views.initialized.remove(&scope);
        app.shared_views.baseline.remove(&scope);
        let (tx, rx) = async_channel::unbounded();
        app.workspace_model_tx = tx;
        drop(app.reconcile_shared_views());
        assert!(app.shared_views.initialized.contains(&scope));
        assert_eq!(app.card_state(pane.id).unwrap().kind, pane.kind);
        assert!(!std::iter::from_fn(|| rx.try_recv().ok()).any(
            |edit| matches!(edit, ModelEdit::SeedView { scope: seeded, .. } if seeded == scope)
        ));
    }

    #[test]
    fn many_matching_populated_views_publish_only_the_genuine_migration() {
        let (mut app, source, _, template) = channel_move_fixture();
        let channel_template = app
            .durable
            .channels
            .iter()
            .find(|channel| channel.id == source)
            .unwrap()
            .clone();
        let model_template = app.workspace_model.channel(source).unwrap().clone();
        let mut migration = None;
        for index in 0..97 {
            let channel_id = ChannelId::new();
            let mut pane = template.clone();
            pane.id = CardId::new();
            pane.title = format!("matching pane {index}");
            if let CardKind::Browser(browser) = &mut pane.kind {
                browser.id = BrowserId::new();
            }
            let mut card = app.card_state(template.id).unwrap().clone();
            card.id = pane.id;
            card.title.clone_from(&pane.title);
            card.kind.clone_from(&pane.kind);
            let mut channel = channel_template.clone();
            channel.id = channel_id;
            channel.cards = vec![pane.id];
            channel.focused = Some(pane.id);
            let durable = Arc::make_mut(&mut app.durable);
            durable.cards.push(card);
            durable.channels.push(channel);
            app.cards.insert(pane.id, CardRuntime::Completed);
            let mut shared_channel = model_template.clone();
            shared_channel.id = channel_id;
            app.workspace_model.channels.push(shared_channel);
            let mut view = View::empty(channel_id);
            if index == 96 {
                migration = Some(view.scope.clone());
            } else {
                view.panes.push(pane);
            }
            app.workspace_model.views.push(view);
        }
        let (tx, rx) = async_channel::unbounded();
        app.workspace_model_tx = tx;
        drop(app.reconcile_shared_views());
        let seeds = std::iter::from_fn(|| rx.try_recv().ok())
            .filter_map(|edit| {
                if let ModelEdit::SeedView { scope, .. } = edit {
                    Some(scope)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        assert_eq!(seeds, [migration.unwrap()]);
    }

    #[test]
    fn exact_chat_admission_tracks_attachment_without_changing_source_or_draft() {
        let (mut app, source, _, pane) = retained_chat_move_fixture();
        let scope = Scope::Channel(source);
        app.shared_views.initialized.remove(&scope);
        app.shared_views.baseline.remove(&scope);
        app.shared_views.attached.remove(&pane.id);
        app.chat_ui = super::super::chat_ui::State::with_drafts(HashMap::from([(
            pane.id,
            "Saved unsent draft  ".into(),
        )]));
        let (tx, rx) = async_channel::unbounded();
        app.workspace_model_tx = tx;
        drop(app.reconcile_shared_views());
        assert!(app.shared_views.initialized.contains(&scope));
        assert!(app.shared_views.attached.contains(&pane.id));
        assert_eq!(app.card_state(pane.id).unwrap().kind, pane.kind);
        assert_eq!(
            app.persistent_workspace()
                .chat_drafts
                .get(&pane.id)
                .map(String::as_str),
            Some("Saved unsent draft  ")
        );
        assert!(!std::iter::from_fn(|| rx.try_recv().ok()).any(
            |edit| matches!(edit, ModelEdit::SeedView { scope: seeded, .. } if seeded == scope)
        ));
    }

    #[test]
    fn scoped_admission_preserves_unrelated_active_channel_snapshot() {
        let (mut app, source, destination, pane) = channel_move_fixture();
        app.active_workspace = Some("/repo".into());
        let mut next = pane.clone();
        next.id = CardId::new();
        if let CardKind::Browser(browser) = &mut next.kind {
            browser.id = BrowserId::new();
        }
        app.workspace_model
            .views
            .iter_mut()
            .find(|view| view.scope == Scope::Channel(source))
            .unwrap()
            .panes
            .push(next.clone());
        drop(app.reconcile_shared_views());
        let saved = app
            .durable
            .channels
            .iter()
            .find(|channel| channel.id == source)
            .unwrap()
            .clone();
        let _ = app.workspace.reorder(next.id, 0).unwrap();
        let _ = app.workspace.focus_without_scrolling(next.id);
        let selected = HashSet::from([Scope::Channel(destination)]);
        app.shared_views.initialized.remove(&Scope::Channel(destination));
        app.shared_views.baseline.remove(&Scope::Channel(destination));

        app.admit_existing_views_in(Some(&selected));

        assert!(app.shared_views.initialized.contains(&Scope::Channel(destination)));
        let current = app
            .durable
            .channels
            .iter()
            .find(|channel| channel.id == source)
            .unwrap();
        assert_eq!(current.cards, saved.cards, "admission is a read-only eligibility check");
        assert_eq!(current.focused, saved.focused);
        assert_eq!(app.workspace.order().collect::<Vec<_>>(), [next.id, pane.id]);
        assert_eq!(app.workspace.focused(), Some(next.id));
    }

    #[test]
    fn populated_admission_checks_the_current_strip_order_before_equality() {
        let (mut app, source, _, pane) = channel_move_fixture();
        app.active_workspace = Some("/repo".into());
        let mut next = pane.clone();
        next.id = CardId::new();
        if let CardKind::Browser(browser) = &mut next.kind {
            browser.id = BrowserId::new();
        }
        let scope = Scope::Channel(source);
        app.workspace_model
            .views
            .iter_mut()
            .find(|view| view.scope == scope)
            .unwrap()
            .panes
            .push(next.clone());
        drop(app.reconcile_shared_views());
        app.shared_views.initialized.remove(&scope);
        app.shared_views.baseline.remove(&scope);
        let _ = app.workspace.reorder(next.id, 0).unwrap();
        let (tx, rx) = async_channel::unbounded();
        app.workspace_model_tx = tx;
        drop(app.reconcile_shared_views());
        assert!(!app.shared_views.initialized.contains(&scope));
        assert_eq!(
            app.workspace.order().collect::<Vec<_>>(),
            [next.id, pane.id]
        );
        assert!(std::iter::from_fn(|| rx.try_recv().ok()).any(|edit|
            matches!(edit, ModelEdit::SeedView { scope: seeded, panes, .. }
                if seeded == scope && panes.iter().map(|pane| pane.id).collect::<Vec<_>>() == [next.id, pane.id])));
    }

    #[test]
    fn populated_admission_does_not_substitute_logical_source_equality() {
        let (mut app, source, _, pane) = channel_move_fixture();
        let scope = Scope::Channel(source);
        app.shared_views.initialized.remove(&scope);
        app.shared_views.baseline.remove(&scope);
        let CardKind::Browser(browser) = &mut app.card_state_mut(pane.id).unwrap().kind else {
            panic!("browser fixture")
        };
        browser.current_url = "https://example.test/changed".parse().unwrap();
        app.admit_existing_views_without_migration();
        assert!(!app.shared_views.initialized.contains(&scope));
        app.card_state_mut(pane.id).unwrap().kind = pane.kind.clone();
        app.shared_views.retained.insert(pane.id);
        app.admit_existing_views_without_migration();
        assert!(!app.shared_views.initialized.contains(&scope));
    }

    #[test]
    fn authoritative_empty_views_do_not_round_trip_an_empty_migration() {
        let (mut app, existing, absent, _) = fixture();
        app.shared_views.initialized.clear();
        app.workspace_model.views = vec![View::empty(existing)];
        let (tx, rx) = async_channel::unbounded();
        app.workspace_model_tx = tx;
        drop(app.reconcile_shared_views());
        let edits = std::iter::from_fn(|| rx.try_recv().ok()).collect::<Vec<_>>();
        assert!(
            !edits.iter().any(|edit| matches!(edit,
            ModelEdit::SeedView { scope: Scope::Channel(id), .. } if *id == existing)),
            "the host already holds this empty view; no migration or receipt is needed"
        );
        assert!(
            app.shared_views
                .initialized
                .contains(&Scope::Channel(existing))
        );
        assert!(
            edits.iter().any(|edit| matches!(edit,
            ModelEdit::SeedView { scope: Scope::Channel(id), .. } if *id == absent)),
            "an absent view still needs the host to create its authority"
        );
    }

    #[test]
    fn empty_view_admission_keeps_local_members_and_pending_intentions_behind_the_seed() {
        let (mut app, source, destination, pane) = channel_move_fixture();
        let source = Scope::Channel(source);
        let destination = Scope::Channel(destination);
        app.shared_views.initialized.clear();
        app.workspace_model
            .views
            .iter_mut()
            .for_each(|view| view.panes.clear());
        let operation = uuid::Uuid::new_v4();
        app.shared_views.pending.push((
            operation,
            destination.clone(),
            Change::Remove { id: CardId::new() },
        ));
        app.admit_existing_views_without_migration();
        assert!(
            !app.shared_views.initialized.contains(&source),
            "local member still needs migration"
        );
        assert!(
            !app.shared_views.initialized.contains(&destination),
            "pending intention still needs its receipt"
        );
        assert_eq!(app.local_view_cards(&source), vec![pane.id]);
        app.shared_views.pending.clear();
        app.shared_views.seeding.insert(destination.clone());
        app.admit_existing_views_without_migration();
        assert!(
            !app.shared_views.initialized.contains(&destination),
            "an in-flight seed still owns its barrier"
        );
    }

    #[test]
    fn an_unprojected_worktree_default_remains_a_local_migration_intention() {
        let (mut app, _, _, pane) = channel_move_fixture_with_kind(Some(moving_terminal_kind()));
        let repository = PathBuf::from("/repo/.git");
        let path = PathBuf::from("/repo/default-worktree");
        let scope = Scope::Worktree {
            repository: repository.clone(),
            path: path.clone(),
        };
        app.sidebar_model.projects.push(SidebarProject::new(
            ProjectId::new(),
            RepositoryId::new(),
            "repo".into(),
            repository,
            vec![],
            vec![],
        ));
        app.known_worktree_roots.insert(path.clone());
        let durable = Arc::make_mut(&mut app.durable);
        for channel in &mut durable.channels {
            channel.cards.retain(|id| *id != pane.id);
        }
        durable.card_scopes.insert(pane.id, path);
        let CardKind::Terminal(session) = pane.kind else {
            panic!("terminal fixture")
        };
        app.shared_views.defaults.insert(pane.id, session);
        app.workspace_model.views.push(View::empty(scope.clone()));
        assert_eq!(app.local_view_cards(&scope), [pane.id]);
        assert!(
            app.local_view_panes(&scope).is_empty(),
            "default is withheld from pane publication"
        );
        app.admit_existing_views_without_migration();
        assert!(
            !app.shared_views.initialized.contains(&scope),
            "membership preserves the default's seed barrier"
        );
    }

    #[test]
    fn empty_view_admission_requires_registry_authority() {
        let (mut app, existing, _, _) = fixture();
        let scope = Scope::Channel(existing);
        app.shared_views.initialized.clear();
        app.workspace_model.identity = crate::workspace_model::NO_REGISTRY;
        app.workspace_model.views = vec![View::empty(scope.clone())];
        app.admit_existing_views_without_migration();
        assert!(!app.shared_views.initialized.contains(&scope));
    }

    #[test]
    fn seed_ack_projects_only_its_scope_and_preserves_unrelated_pending_edits() {
        let (mut app, source, destination, pane) = channel_move_fixture();
        let source = Scope::Channel(source);
        let destination = Scope::Channel(destination);
        let unrelated = uuid::Uuid::new_v4();
        let change = Change::Remove { id: CardId::new() };
        app.shared_views
            .pending
            .push((unrelated, destination.clone(), change.clone()));
        app.shared_views.seeding.insert(source.clone());
        app.shared_views.initialized.remove(&source);
        let (tx, rx) = async_channel::unbounded();
        app.workspace_model_tx = tx;
        app.shared_views.local_projection_count.set(0);
        app.acknowledge_view_edit(source.clone(), None);
        assert_eq!(
            app.shared_views.local_projection_count.get(),
            0,
            "a seed receipt must not reconstruct unrelated scopes"
        );
        assert!(app.shared_views.initialized.contains(&source));
        assert!(!app.shared_views.seeding.contains(&source));
        assert!(
            app.shared_views
                .pending
                .contains(&(unrelated, destination.clone(), change))
        );
        assert!(
            app.shared_views.baseline[&source]
                .iter()
                .any(|item| item.id == pane.id)
        );
        assert!(
            !std::iter::from_fn(|| rx.try_recv().ok()).any(|edit| matches!(edit,
            ModelEdit::ChangeView { scope, .. } | ModelEdit::SeedView { scope, .. }
            if scope == destination))
        );
    }

    #[test]
    fn reused_projection_seeds_new_scopes_and_drains_edits_after_equal_ack() {
        use super::super::workspace_model_sync::ModelUpdate;
        let (mut app, source, destination, pane) = channel_move_fixture();
        let mut next = pane.clone();
        next.id = CardId::new();
        if let CardKind::Browser(browser) = &mut next.kind {
            browser.id = BrowserId::new();
        }
        let durable = Arc::make_mut(&mut app.durable);
        durable.cards.push(CardState {
            id: next.id,
            generation: 0,
            title: next.title.clone(),
            width_fraction: next.width_fraction,
            existence: Existence::Open,
            kind: next.kind.clone(),
        });
        durable
            .channels
            .iter_mut()
            .find(|channel| channel.id == destination)
            .unwrap()
            .cards = vec![next.id];
        app.cards.insert(next.id, CardRuntime::Completed);
        app.shared_views
            .initialized
            .remove(&Scope::Channel(destination));
        let (tx, rx) = async_channel::unbounded();
        app.workspace_model_tx = tx;
        drop(app.reconcile_shared_views());
        let requests = std::iter::from_fn(|| rx.try_recv().ok()).collect::<Vec<_>>();
        assert!(requests.iter().any(|edit| matches!(edit,
            ModelEdit::SeedView { scope, panes, .. } if scope == &Scope::Channel(destination) && panes == &vec![next.clone()])));
        app.card_state_mut(next.id).unwrap().title = "While seed is pending".into();
        app.persist();
        assert!(
            !std::iter::from_fn(|| rx.try_recv().ok())
                .any(|edit| matches!(edit, ModelEdit::ChangeView { .. }))
        );
        let mut model = app.workspace_model.clone();
        model
            .views
            .iter_mut()
            .find(|view| view.scope == Scope::Channel(destination))
            .unwrap()
            .panes = vec![next.clone()];
        // A seed acknowledgement remains semantically required even with no
        // newer outer sequence: it initializes and drains the retained edit.
        drop(app.apply_workspace_model_update(ModelUpdate::ViewApplied {
            scope: Scope::Channel(destination),
            operation: None,
            model,
        }));
        let requests = std::iter::from_fn(|| rx.try_recv().ok()).collect::<Vec<_>>();
        assert!(requests.iter().any(|edit| matches!(edit,
            ModelEdit::ChangeView { scope, change: Change::Update { pane, .. }, .. }
            if scope == &Scope::Channel(destination) && pane.id == next.id && pane.title == "While seed is pending")));
        assert_eq!(
            app.card_state(next.id).unwrap().title,
            "While seed is pending"
        );
        // Normal local persistence still snapshots an active strip and emits
        // its changed width; reuse is confined to completed reconciliation.
        let _ = app
            .workspace
            .set_card_width(pane.id, CardWidth::Fraction(0.68))
            .unwrap();
        app.card_state_mut(pane.id).unwrap().width_fraction = 0.68;
        app.persist();
        assert!(std::iter::from_fn(|| rx.try_recv().ok()).any(|edit| matches!(edit,
            ModelEdit::ChangeView { scope, change: Change::Update { pane: changed, .. }, .. }
            if scope == Scope::Channel(source) && changed.id == pane.id && changed.width_fraction == 0.68)));
    }

    #[test]
    fn reconciliation_settles_an_unsaved_strip_order_before_taking_its_baseline() {
        let (mut app, source, _, pane) = channel_move_fixture();
        let mut next = pane.clone();
        next.id = CardId::new();
        if let CardKind::Browser(browser) = &mut next.kind {
            browser.id = BrowserId::new();
        }
        app.workspace_model
            .views
            .iter_mut()
            .find(|view| view.scope == Scope::Channel(source))
            .unwrap()
            .panes
            .push(next.clone());
        drop(app.reconcile_shared_views());
        let (tx, rx) = async_channel::unbounded();
        app.workspace_model_tx = tx;
        let _ = app.workspace.reorder(next.id, 0).unwrap();
        // The next echo projects the view over the mounted strip before the
        // snapshot, so the snapshot has no reorder for the baseline to absorb.
        drop(app.reconcile_shared_views());
        assert_eq!(
            app.workspace.order().collect::<Vec<_>>(),
            vec![pane.id, next.id]
        );
        assert_eq!(
            app.durable
                .channels
                .iter()
                .find(|channel| channel.id == source)
                .unwrap()
                .cards,
            vec![pane.id, next.id]
        );
        assert!(
            !std::iter::from_fn(|| rx.try_recv().ok())
                .any(|edit| matches!(edit, ModelEdit::ChangeView { .. }))
        );
    }

    /// Scope discovery by scanning every root and project worktree per card
    /// and path: the definition the indexed discovery must reproduce.
    fn reference_view_scopes(app: &Cagents) -> Vec<Scope> {
        let mut scopes = app
            .durable
            .channels
            .iter()
            .map(|channel| Scope::Channel(channel.id))
            .collect::<Vec<_>>();
        scopes.extend(app.workspace_model.views.iter().filter_map(|view| {
            match &view.scope {
                Scope::Worktree { repository, .. }
                    if app
                        .sidebar_model
                        .projects
                        .iter()
                        .any(|project| &project.repository == repository) =>
                {
                    Some(view.scope.clone())
                }
                _ => None,
            }
        }));
        let paths = app
            .known_worktree_roots
            .iter()
            .map(PathBuf::as_path)
            .chain(app.active_workspace.as_deref())
            .chain(
                app.durable
                    .cards
                    .iter()
                    .filter_map(|card| app.card_worktree(card)),
            );
        scopes.extend(paths.filter_map(|path| app.worktree_view_scope(path)));
        let mut seen = HashSet::new();
        scopes.retain(|scope| seen.insert(scope.clone()));
        scopes
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
        let scopes = reference_view_scopes(&app);
        let reference = scopes
            .iter()
            .map(|scope| (scope.clone(), app.local_view_panes(scope)))
            .collect::<HashMap<_, _>>();
        let reference_elapsed = started.elapsed();
        assert_eq!(batched.scopes, scopes);
        assert_eq!(app.local_view_scopes(), scopes);
        assert_eq!(batched.panes, reference);
        for card in app.durable.cards.iter().step_by(17) {
            let listing = scopes
                .iter()
                .filter(|scope| app.local_view_cards(scope).contains(&card.id))
                .cloned()
                .collect::<Vec<_>>();
            assert_eq!(app.local_scopes_listing(card.id), listing);
        }
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
        let observed = app.ended_terminal(pane.id).unwrap().continuity.clone();

        // The registry's pane descriptor can predate the runtime event, even
        // while its layout revision is newer. It does not revoke that event.
        drop(app.project_shared_view(&view, &owned));
        assert_eq!(app.ended_terminal(pane.id).unwrap().continuity, observed);

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
        let saved = app.ended_terminal(pane.id).unwrap();
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
        app.set_residency(pane.id, Residency::Hydrating { epoch: 1 });
        assert!(!app.terminal_start_in_flight(pane.id));

        // Projection admits a waiting pane as Cold. Evidence must come from
        // before that change, while the old exit still belongs to a relaunch.
        drop(app.project_shared_view(&view, &owned));
        assert_eq!(app.card_state(pane.id).unwrap().kind, pane.kind);
        assert!(app.ended_terminal(pane.id).is_none());
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
            preserve_observed_terminal_exit(app.ended_terminal(id), &mut kind);
            assert_eq!(kind, expected);
        }
        app.set_residency(id, Residency::Hydrating { epoch: 1 });
        let mut kind = CardKind::Terminal(missing);
        let expected = kind.clone();
        preserve_observed_terminal_exit(app.ended_terminal(id), &mut kind);
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
        assert_eq!(*app.residency(pane.id), Residency::Hot);
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
        assert_eq!(*app.residency(pane.id), Residency::Hot);

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

    /// A chat link to `src/lib.rs:12` opens a pane on `src`, which is no
    /// project's root. Its own echo must keep the viewer that holds the line.
    #[test]
    fn a_file_pane_outside_a_project_root_keeps_its_viewer_through_its_echo() {
        let (mut app, channel, _, mut pane) = fixture();
        let root = tempfile::tempdir().unwrap();
        let folder = root.path().join("src");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("lib.rs"), "fn a() {}\n").unwrap();
        let descriptor = RepositoryView {
            project: ProjectId::new(),
            repository: folder.clone(),
            worktree: folder,
            selected_file: Some("lib.rs".into()),
        };
        pane.kind = CardKind::Repository(descriptor.clone());
        let mut view = View::empty(channel);
        view.panes.push(pane.clone());
        drop(app.project_shared_view(&view, &HashSet::from([pane.id])));
        let mut runtime = RepositoryRuntime::new_directory(descriptor.clone());
        drop(runtime.initial_load(pane.id));
        drop(runtime.preview_fragment(pane.id, "L12"));
        let runtime = Box::new(runtime);
        let viewer = std::ptr::from_ref::<RepositoryRuntime>(&runtime);
        app.cards.insert(pane.id, CardRuntime::Repository(runtime));
        view.panes[0].kind = views::shared_kind(&pane.kind);
        drop(app.project_shared_view(&view, &HashSet::from([pane.id])));
        let Some(CardRuntime::Repository(runtime)) = app.cards.get(&pane.id) else {
            panic!("the file pane must stay attached");
        };
        assert!(
            std::ptr::eq(viewer, &**runtime),
            "the pane's own echo must not replace the viewer waiting to open line 12"
        );
        assert_eq!(app.card_state(pane.id).unwrap().kind, pane.kind);
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
    fn a_refused_terminal_creation_closes_instead_of_loading_forever() {
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
        let session = fresh_terminal_session(LaunchRecipe::shell(PathBuf::from("/repo")).unwrap());
        drop(app.insert_terminal_card(session, Some((0, 0))));
        let id = app.workspace.focused().unwrap();
        assert!(app.durable.pending_terminal_creations.contains(&id));

        app.refuse_shared_view_edit(&Scope::Channel(a), id.as_uuid());
        for _ in 0..2 {
            drop(app.reconcile_shared_views());
        }

        assert!(!app.card_state(id).unwrap().existence.is_open());
        assert!(!app.shared_views.retained.contains(&id));
        assert!(!app.durable.local_only_cards.contains(&id));
        assert!(app.terminal_starts.lock().unwrap().is_empty());
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
            app.workspace_model.views[0]
                .apply(&Change::Remove { id: pane.id })
                .unwrap();
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
        drop(app.apply_viewport(update, false, DurableChange::None));
        assert_eq!(app.durable.focused, Some(second.id));
        crate::persistence::validate_workspace(&app.persistent_workspace()).unwrap();

        view.apply(&Change::Remove { id: second.id }).unwrap();
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
    fn an_open_pane_losing_its_local_placement_does_not_publish_a_removal() {
        let (mut app, a, _, pane) = fixture();
        app.workspace_model.views = vec![View {
            panes: vec![pane.clone()],
            revision: 1,
            ..View::empty(a)
        }];
        drop(app.reconcile_shared_views());
        assert!(app.card_state(pane.id).unwrap().existence.is_open());
        let (sender, published) = async_channel::unbounded();
        app.workspace_model_tx = sender;
        for channel in &mut Arc::make_mut(&mut app.durable).channels {
            channel.cards.retain(|id| *id != pane.id);
        }
        app.publish_view_changes();
        assert!(
            !std::iter::from_fn(|| published.try_recv().ok()).any(|edit| matches!(
                edit,
                ModelEdit::ChangeView {
                    change: Change::Remove { .. },
                    ..
                }
            )),
            "only a close may tell every window to close the pane"
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
        for id in [first.id, second.id] {
            app.workspace_model.views[0]
                .apply(&Change::Remove { id })
                .unwrap();
        }
        drop(app.reconcile_shared_views());
        assert!(!app.card_state(first.id).unwrap().existence.is_open());
    }

    fn attention_terminal_source() -> TerminalSession {
        let mut session =
            fresh_terminal_session(LaunchRecipe::shell(PathBuf::from("/repo")).unwrap());
        session.continuity = TerminalContinuity::Live {
            host_session_id: crate::model::HostSessionId::new("attention-owner"),
            boot_id: crate::model::BootId::new("attention-boot"),
            host_incarnation: crate::model::HostIncarnation::new("attention-host"),
        };
        session
    }

    /// A terminal's reported title is published with its deferred save, not
    /// with each view acknowledgment. Until then a view edit from another
    /// window, carrying the older shared title, must not take it back.
    #[test]
    fn a_reported_title_awaiting_its_save_survives_shared_view_edits() {
        let (mut app, channel, _, _) = fixture();
        app.active_workspace = Some(PathBuf::from("/repo"));
        Arc::make_mut(&mut app.durable).active_channel = Some(channel);
        drop(app.insert_terminal_card(attention_terminal_source(), None));
        let card = app.workspace.focused().unwrap();
        let shared = Pane::from_card(app.card_state(card).unwrap(), None).unwrap();
        let mut view = View::empty(channel);
        view.panes.push(shared.clone());
        app.workspace_model.views = vec![view.clone()];
        drop(app.reconcile_shared_views());
        app.shared_views.pending.clear();
        let publishes_title = |app: &Cagents| {
            app.shared_views.pending.iter().any(|(_, scope, change)| {
                *scope == Scope::Channel(channel)
                    && matches!(change, Change::Update { pane, .. } if pane.id == card && pane.title == "build done")
            })
        };

        assert!(app.apply_reported_title(card, "build done".into()));
        drop(app.save_reported_titles_soon(card));
        let mut wider = shared.clone();
        wider.width_fraction = 0.75;
        view.apply(&Change::Update {
            before: shared,
            pane: wider,
        })
        .unwrap();
        app.workspace_model.views = vec![view];
        drop(app.reconcile_shared_views());
        let state = app.card_state(card).unwrap();
        assert_eq!(state.title, "build done");
        assert_eq!(
            state.width_fraction, 0.75,
            "the other window's edit applies"
        );
        assert_eq!(
            app.workspace.card(card).unwrap().accessible_name,
            "build done",
            "the pane keeps its newer name"
        );
        assert!(
            !publishes_title(&app),
            "an acknowledgment does not publish it"
        );
        assert!(app.reported_title_save.cards.contains(&card));

        drop(app.update(Message::ReportedTitleSaveDue));
        assert!(publishes_title(&app), "its save publishes it");
        assert!(app.reported_title_save.cards.is_empty());
        assert_eq!(app.card_state(card).unwrap().title, "build done");
    }

    /// Every save other than reconciliation's own publishes held titles, so
    /// exiting, which saves, never leaves one unpublished.
    #[test]
    fn a_save_publishes_held_reported_titles() {
        let (mut app, channel, _, _) = fixture();
        app.active_workspace = Some(PathBuf::from("/repo"));
        Arc::make_mut(&mut app.durable).active_channel = Some(channel);
        drop(app.insert_terminal_card(attention_terminal_source(), None));
        let card = app.workspace.focused().unwrap();
        let shared = Pane::from_card(app.card_state(card).unwrap(), None).unwrap();
        let mut view = View::empty(channel);
        view.panes.push(shared);
        app.workspace_model.views = vec![view];
        drop(app.reconcile_shared_views());
        app.shared_views.pending.clear();

        assert!(app.apply_reported_title(card, "build done".into()));
        drop(app.save_reported_titles_soon(card));
        app.persist();
        assert!(app.reported_title_save.cards.is_empty());
        assert!(app.shared_views.pending.iter().any(|(_, _, change)| {
            matches!(change, Change::Update { pane, .. } if pane.id == card && pane.title == "build done")
        }));
    }

    /// A name given in another window replaces a held reported title, which
    /// must then not be published back over that window's rename.
    #[test]
    fn a_shared_rename_releases_a_held_reported_title() {
        let (mut app, channel, _, _) = fixture();
        app.active_workspace = Some(PathBuf::from("/repo"));
        Arc::make_mut(&mut app.durable).active_channel = Some(channel);
        drop(app.insert_terminal_card(attention_terminal_source(), None));
        let card = app.workspace.focused().unwrap();
        let shared = Pane::from_card(app.card_state(card).unwrap(), None).unwrap();
        let mut view = View::empty(channel);
        view.panes.push(shared.clone());
        app.workspace_model.views = vec![view.clone()];
        drop(app.reconcile_shared_views());
        app.shared_views.pending.clear();

        assert!(app.apply_reported_title(card, "build done".into()));
        drop(app.save_reported_titles_soon(card));
        let mut renamed = shared.clone();
        renamed.user_title = Some("Release".into());
        renamed.title = "Release".into();
        view.apply(&Change::Update {
            before: shared,
            pane: renamed,
        })
        .unwrap();
        app.workspace_model.views = vec![view];
        drop(app.reconcile_shared_views());

        assert!(app.reported_title_save.cards.is_empty());
        assert_eq!(app.card_state(card).unwrap().title, "Release");
        assert_eq!(app.workspace.card(card).unwrap().accessible_name, "Release");
        app.shared_views.pending.clear();
        drop(app.update(Message::ReportedTitleSaveDue));
        assert!(
            !app.shared_views.pending.iter().any(|(_, _, change)| {
                matches!(change, Change::Update { pane, .. } if pane.title == "build done")
            }),
            "the replaced reported title is not published"
        );
    }

    /// A pane moving between views in the save that captures it is still in
    /// its source's baseline. The move carries no title, so the held title
    /// must not enter the destination's baseline unpublished.
    #[test]
    fn a_held_reported_title_stays_held_for_a_pane_moving_between_views() {
        let (mut app, source, destination, mut pane) = fixture();
        pane.title = "shared title".into();
        app.shared_views
            .baseline
            .insert(Scope::Channel(source), vec![pane.clone()]);
        let mut local = pane.clone();
        local.title = "held title".into();
        app.reported_title_save.cards.insert(pane.id);
        app.hold_reported_title(&Scope::Channel(destination), &mut local);
        assert_eq!(local.title, "shared title");

        app.reported_title_save.cards.clear();
        local.title = "held title".into();
        app.hold_reported_title(&Scope::Channel(destination), &mut local);
        assert_eq!(local.title, "held title", "a published title is not held");
    }

    #[test]
    fn attention_passive_shared_removal_forgets_closed_unread_without_retaining_a_ghost() {
        let (mut app, channel, _, _) = fixture();
        app.active_workspace = Some(PathBuf::from("/repo"));
        Arc::make_mut(&mut app.durable).active_channel = Some(channel);
        drop(app.insert_terminal_card(attention_terminal_source(), None));
        let closed = app.workspace.focused().unwrap();
        drop(app.insert_terminal_card(attention_terminal_source(), None));
        let other = app.workspace.focused().unwrap();
        let retained = Pane::from_card(app.card_state(other).unwrap(), None).unwrap();
        let project = ProjectId::new();
        let worktree = crate::model::WorktreeId::new();
        app.sidebar_state
            .apply(&SidebarAction::SetFilter(sidebar::SidebarFilter::Attention));
        app.sidebar_state
            .observe_agent_update(closed, project, worktree);
        app.sidebar_state.observe_card_update(other);
        app.sidebar_state.set_agent_activity(
            closed,
            project,
            worktree,
            crate::agent::Activity {
                provider: crate::agent::Provider::Codex,
                state: crate::agent::ActivityState::Waiting,
            },
        );
        app.shared_views.pending.clear();
        let mut view = View::empty(channel);
        view.panes.push(retained);
        view.retired.push(closed);
        app.workspace_model.views = vec![view];
        drop(app.reconcile_shared_views());
        assert!(!app.card_state(closed).unwrap().existence.is_open());
        assert!(app.card_state(other).unwrap().existence.is_open());
        assert!(!app.sidebar_state.has_card_update(closed));
        assert!(!app.sidebar_state.has_agent_update(project, worktree));
        assert!(app.sidebar_state.has_card_update(other));
    }

    #[test]
    fn attention_passive_shared_close_forgets_surface_without_acknowledging_unseen_notice() {
        let (mut app, channel, _, _) = fixture();
        app.refresh_ui.focused = false;
        app.active_workspace = Some(PathBuf::from("/repo"));
        Arc::make_mut(&mut app.durable).active_channel = Some(channel);
        let session = attention_terminal_source();
        drop(app.insert_terminal_card(session.clone(), None));
        let card = app.workspace.focused().unwrap();
        let id = uuid::Uuid::new_v4();
        let snapshot = crate::notifications::Snapshot {
            diagnostic: None,
            notices: vec![crate::notifications::Notice {
                id,
                delivered: true,
                acknowledged: false,
                window: None,
                event: crate::notifications::Event {
                    content: None,
                    target: crate::notifications::Target::Session(session.id),
                    source_session: Some(session.id),
                    provider: Some(crate::agent::Provider::Codex),
                    directory: Some(session.launch.working_directory.clone()),
                    destination: None,
                    evidence: crate::notifications::Evidence {
                        key: "unseen-before-passive-close".into(),
                        kind: crate::notifications::Kind::Succeeded,
                    },
                },
            }],
        };
        drop(app.update_notifications(notification_ui::Action::Snapshot(snapshot.clone())));
        assert!(app.sidebar_state.has_card_update(card));
        assert!(!app.notification_ui.acknowledged_for_test(id));
        app.shared_views.pending.clear();
        let mut view = View::empty(channel);
        view.retired.push(card);
        app.workspace_model.views = vec![view];
        drop(app.reconcile_shared_views());
        assert!(!app.card_state(card).unwrap().existence.is_open());
        assert!(!app.sidebar_state.has_card_update(card));
        assert!(
            !app.notification_ui.acknowledged_for_test(id),
            "surface retirement is not evidence the user read its content"
        );
        assert!(!app.notification_ui.snapshot.notices[0].acknowledged);
        // The durable journal may keep the unseen notice. A later broker
        // snapshot cannot recreate a live sidebar entry for its closed pane.
        drop(app.update_notifications(notification_ui::Action::Snapshot(snapshot)));
        assert!(!app.sidebar_state.has_card_update(card));
        assert!(!app.notification_ui.acknowledged_for_test(id));
    }

    #[test]
    fn same_card_session_replacement_retires_old_attention_without_reading_its_notice() {
        use crate::agent::{Activity, ActivityState, Provider};

        let (mut app, channel, _, mut original) = fixture();
        app.refresh_ui.focused = false;
        app.active_workspace = Some(PathBuf::from("/repo"));
        Arc::make_mut(&mut app.durable).active_channel = Some(channel);
        let old_session = attention_terminal_source();
        original.kind = CardKind::Terminal(old_session.clone());
        let mut unrelated = original.clone();
        unrelated.id = CardId::new();
        unrelated.kind = CardKind::Terminal(attention_terminal_source());
        let mut view = View::empty(channel);
        view.panes = vec![original.clone(), unrelated.clone()];
        app.workspace_model.views = vec![view.clone()];
        drop(app.project_shared_view(&view, &HashSet::from([original.id, unrelated.id])));
        assert!(app.terminal_start_in_flight(original.id));
        assert!(matches!(
            app.cards.get(&original.id),
            Some(CardRuntime::Loading)
        ));

        let notice = uuid::Uuid::new_v4();
        let snapshot = crate::notifications::Snapshot {
            diagnostic: None,
            notices: vec![crate::notifications::Notice {
                id: notice,
                delivered: true,
                acknowledged: false,
                window: None,
                event: crate::notifications::Event {
                    content: None,
                    target: crate::notifications::Target::Session(old_session.id),
                    source_session: Some(old_session.id),
                    provider: Some(Provider::Codex),
                    directory: Some(old_session.launch.working_directory.clone()),
                    destination: None,
                    evidence: crate::notifications::Evidence {
                        key: "unseen-before-same-card-replacement".into(),
                        kind: crate::notifications::Kind::Succeeded,
                    },
                },
            }],
        };
        drop(app.update_notifications(notification_ui::Action::Snapshot(snapshot.clone())));
        let waiting = Activity {
            provider: Provider::Codex,
            state: ActivityState::Waiting,
        };
        app.observe_agent_activity_for_card(original.id, Some(waiting));
        app.observe_agent_update_for_card(unrelated.id);
        assert_eq!(app.sidebar_state.card_activity(original.id), Some(waiting));
        assert!(app.sidebar_state.has_card_update(original.id));
        assert!(app.sidebar_state.has_card_update(unrelated.id));
        app.shared_views.pending.clear();

        // A genuine presentation-only view update must preserve the source's
        // existing attention while the attachment task is still outstanding.
        let mut metadata = original.clone();
        metadata.title = "same terminal, renamed".into();
        view.apply(&Change::Update {
            before: original.clone(),
            pane: metadata.clone(),
        })
        .unwrap();
        app.workspace_model.views = vec![view.clone()];
        drop(app.reconcile_shared_views());
        assert_eq!(app.sidebar_state.card_activity(original.id), Some(waiting));
        assert!(app.sidebar_state.has_card_update(original.id));

        // The same pane ID may subsequently name an entirely new terminal.
        // Its real Loading/error projection must not borrow its predecessor's
        // activity or unread state, even before the new start can settle.
        let replacement_session = attention_terminal_source();
        assert_ne!(replacement_session.id, old_session.id);
        let mut replacement = metadata.clone();
        replacement.kind = CardKind::Terminal(replacement_session.clone());
        view.apply(&Change::Update {
            before: metadata,
            pane: replacement.clone(),
        })
        .unwrap();
        app.workspace_model.views = vec![view];
        drop(app.reconcile_shared_views());
        assert_eq!(app.card_state(original.id).unwrap().kind, replacement.kind);
        assert!(matches!(
            app.cards.get(&original.id),
            Some(CardRuntime::Loading)
        ));
        assert!(app.terminal_start_in_flight(original.id));
        assert_eq!(app.sidebar_state.card_activity(original.id), None);
        assert!(!app.sidebar_state.has_card_update(original.id));
        assert!(app.sidebar_state.has_card_update(unrelated.id));
        assert!(!app.notification_ui.acknowledged_for_test(notice));
        assert!(!app.notification_ui.snapshot.notices[0].acknowledged);

        // Neither a delayed old attachment failure nor its still-unread
        // journal snapshot can assign old-session news to the replacement.
        drop(app.update(Message::TerminalStarted {
            card: original.id,
            terminal: old_session.id,
            result: Err("old attachment failed after session replacement".into()),
        }));
        assert!(matches!(
            app.cards.get(&original.id),
            Some(CardRuntime::Loading)
        ));
        drop(app.update_notifications(notification_ui::Action::Snapshot(snapshot)));
        assert_eq!(app.sidebar_state.card_activity(original.id), None);
        assert!(!app.sidebar_state.has_card_update(original.id));
        assert!(!app.notification_ui.acknowledged_for_test(notice));

        // Fresh source observations remain usable. This invokes the App's
        // observer contract; no fixture start task or real host is executed.
        app.observe_agent_activity_for_card(original.id, Some(waiting));
        assert_eq!(app.sidebar_state.card_activity(original.id), Some(waiting));
        assert!(app.sidebar_state.has_card_update(original.id));
        drop(app.update(Message::TerminalStarted {
            card: original.id,
            terminal: replacement_session.id,
            result: Err("replacement attachment unavailable".into()),
        }));
        assert!(matches!(
            app.cards.get(&original.id),
            Some(CardRuntime::Failed(_))
        ));
        assert_eq!(app.sidebar_state.card_activity(original.id), None);
        assert!(
            app.sidebar_state.has_card_update(original.id),
            "fresh unread is not read by a runtime failure"
        );
        assert!(app.sidebar_state.has_card_update(unrelated.id));
        assert!(!app.notification_ui.acknowledged_for_test(notice));
    }

    #[test]
    fn attention_alias_adoption_preserves_unread_only_for_the_exact_complete_native_source() {
        for source in 0..5 {
            let (mut app, channel, _, _) = fixture();
            app.active_workspace = Some(PathBuf::from("/repo"));
            Arc::make_mut(&mut app.durable).active_channel = Some(channel);
            drop(app.insert_terminal_card(attention_terminal_source(), None));
            let alias = app.workspace.focused().unwrap();
            app.chat_ui =
                chat_ui::State::with_drafts(HashMap::from([(alias, "retained draft".into())]));
            let mut canonical = Pane::from_card(app.card_state(alias).unwrap(), None).unwrap();
            canonical.id = CardId::new();
            let CardKind::Terminal(current) = &mut canonical.kind else {
                unreachable!()
            };
            match source {
                0 => {}
                1 => {
                    if let TerminalContinuity::Live {
                        host_incarnation, ..
                    } = &mut current.continuity
                    {
                        *host_incarnation = crate::model::HostIncarnation::new("replacement-owner");
                    }
                }
                2 => {
                    current.continuity = TerminalContinuity::RelaunchAvailable {
                        recipe: current.launch.clone(),
                        last_exit: None,
                    }
                }
                3 => {
                    if let TerminalContinuity::Live { boot_id, .. } = &mut current.continuity {
                        *boot_id = crate::model::BootId::new("");
                    }
                }
                4 => {}
                _ => unreachable!(),
            }
            app.sidebar_state.observe_card_update(alias);
            app.shared_views.pending.clear();
            let mut view = View::empty(channel);
            view.panes.push(canonical.clone());
            let duplicate = if source == 4 {
                let mut duplicate = canonical.clone();
                duplicate.id = CardId::new();
                let id = duplicate.id;
                view.panes.push(duplicate);
                Some(id)
            } else {
                None
            };
            app.workspace_model.views = vec![view];
            drop(app.reconcile_shared_views());
            assert!(!app.card_state(alias).unwrap().existence.is_open());
            assert!(app.card_state(canonical.id).unwrap().existence.is_open());
            assert!(!app.sidebar_state.has_card_update(alias));
            assert_eq!(
                app.sidebar_state.has_card_update(canonical.id),
                source == 0,
                "source case{source}"
            );
            if let Some(duplicate) = duplicate {
                assert!(
                    !app.sidebar_state.has_card_update(duplicate),
                    "ambiguous ownership is not first-row routing"
                );
            }
            assert_eq!(
                app.persistent_workspace()
                    .chat_drafts
                    .get(&canonical.id)
                    .map(String::as_str),
                Some("retained draft")
            );
        }
    }

    #[test]
    fn attention_chat_alias_proof_does_not_follow_another_native_thread_or_unknown_identity() {
        let source = attention_terminal_source();
        let proof = source.continuity.live_proof().unwrap();
        let a = crate::chat::Session {
            id: source.id,
            proof: Some(proof),
            terminal: None,
            owned_codex_listener: None,
            native_continuity: Default::default(),
            launch: crate::chat::Launch {
                provider: crate::agent::Provider::Codex,
                session_id: Some("original-thread".into()),
                recipe: source.launch.clone(),
            },
        };
        assert!(same_attention_update_source(
            &CardKind::Chat(a.clone()),
            &CardKind::Chat(a.clone())
        ));
        let mut b = a.clone();
        b.launch.session_id = Some("successor-thread".into());
        assert!(!same_attention_update_source(
            &CardKind::Chat(a.clone()),
            &CardKind::Chat(b)
        ));
        let mut unknown = a.clone();
        unknown.launch.session_id = None;
        assert!(!same_attention_update_source(
            &CardKind::Chat(unknown.clone()),
            &CardKind::Chat(unknown)
        ));
        let mut missing = a.clone();
        missing.proof = None;
        assert!(!same_attention_update_source(
            &CardKind::Chat(missing.clone()),
            &CardKind::Chat(missing)
        ));
    }

    #[test]
    fn repository_refresh_adopts_cross_repository_alias_without_losing_its_draft() {
        let (mut app, source, destination, _) = fixture();
        let source_project = app.durable.channels[0].project_id;
        let destination_project = ProjectId::new();
        Arc::make_mut(&mut app.durable)
            .channels
            .iter_mut()
            .find(|channel| channel.id == destination)
            .unwrap()
            .project_id = destination_project;
        let source_repository = PathBuf::from("/repo/.git");
        let destination_repository = PathBuf::from("/other/.git");
        for (id, repository) in [
            (source_project, source_repository),
            (destination_project, destination_repository.clone()),
        ] {
            app.sidebar_model.projects.push(SidebarProject::new(
                id,
                RepositoryId::new(),
                "repo".into(),
                repository,
                vec![],
                vec![],
            ));
        }
        app.active_workspace = Some(PathBuf::from("/repo"));
        Arc::make_mut(&mut app.durable).active_channel = Some(source);
        let session = fresh_terminal_session(LaunchRecipe::shell(PathBuf::from("/repo")).unwrap());
        drop(app.insert_terminal_card(session, None));
        let alias = app.workspace.focused().unwrap();
        app.chat_ui = chat_ui::State::with_drafts(HashMap::from([(
            alias,
            "saved cross-repository draft  ".into(),
        )]));
        let mut canonical = Pane::from_card(app.card_state(alias).unwrap(), None).unwrap();
        canonical.id = CardId::new();
        app.shared_views.pending.clear();
        let mut view = View::empty(destination);
        view.panes.push(canonical.clone());
        app.workspace_model.views = vec![View::empty(source), view];
        drop(app.reconcile_shared_views_for_repository(&destination_repository, &HashSet::new()));
        assert!(!app.card_state(alias).unwrap().existence.is_open());
        assert!(app.card_state(canonical.id).unwrap().existence.is_open());
        assert!(
            app.durable
                .channels
                .iter()
                .find(|channel| channel.id == source)
                .unwrap()
                .cards
                .iter()
                .all(|id| *id != alias)
        );
        assert!(
            app.durable
                .channels
                .iter()
                .find(|channel| channel.id == destination)
                .unwrap()
                .cards
                .contains(&canonical.id)
        );
        assert!(
            app.shared_views.baseline[&Scope::Channel(source)]
                .iter()
                .all(|pane| pane.id != alias)
        );
        assert!(
            app.shared_views.baseline[&Scope::Channel(destination)]
                .iter()
                .any(|pane| pane.id == canonical.id)
        );
        assert_eq!(
            app.persistent_workspace()
                .chat_drafts
                .get(&canonical.id)
                .map(String::as_str),
            Some("saved cross-repository draft  ")
        );
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
        app.set_residency(id, Residency::Hydrating { epoch: 1 });
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

    /// A window restored with a terminal nobody has opened, while its registry
    /// is still connecting.
    fn restored_unopened_terminal() -> (Cagents, CardId, uuid::Uuid) {
        let (mut app, _, _, _) = fixture();
        let session = fresh_terminal_session(LaunchRecipe::shell(PathBuf::from("/repo")).unwrap());
        drop(app.insert_terminal_card(session, None));
        let id = app.workspace.focused().unwrap();
        let identity = app.workspace_model.identity;
        Arc::make_mut(&mut app.durable).workspace_model_identity = Some(identity);
        app.shared_views = State::for_workspace(&app.durable);
        app.workspace_model = crate::workspace_model::WorkspaceModel::default();
        app.terminal_starts.lock().unwrap().clear();
        app.cards.insert(id, CardRuntime::Loading);
        app.set_exposure(id, Exposure::Offscreen);
        app.set_residency(id, Residency::Hibernated);
        app.lifecycle.register(
            id,
            Exposure::Offscreen,
            Residency::Hibernated,
            Instant::now(),
        );
        (app, id, identity)
    }

    /// #2104: a window restored while its registry is still connecting holds
    /// every saved terminal it may share. Projecting a view admits its own held
    /// terminals again once the registry answers; a terminal the registry lists
    /// in no view has no projection, so it must be released too. It restores
    /// as this window's own saved terminal rather than spinning forever or
    /// waiting for another window to host it.
    #[test]
    fn restored_window_local_terminal_held_for_registry_restores_as_saved_once_it_answers() {
        let (mut app, _, _, _) = fixture();
        let session = fresh_terminal_session(LaunchRecipe::shell(PathBuf::from("/repo")).unwrap());
        drop(app.insert_terminal_card(session, None));
        let id = app.workspace.focused().unwrap();
        assert!(!app.durable.pending_terminal_creations.contains(&id));
        // Restart: the saved window had joined a registry, so its terminal
        // may be shared, and the new process has not heard from one yet.
        let identity = app.workspace_model.identity;
        Arc::make_mut(&mut app.durable).workspace_model_identity = Some(identity);
        app.shared_views = State::for_workspace(&app.durable);
        app.workspace_model = crate::workspace_model::WorkspaceModel::default();
        app.terminal_starts.lock().unwrap().clear();
        app.cards.insert(id, CardRuntime::Loading);
        app.set_residency(id, Residency::Hibernated);
        app.lifecycle
            .register(id, Exposure::Visible, Residency::Hibernated, Instant::now());
        drop(app.reconcile_lifecycle(&HashSet::from([id]), &HashSet::new(), &HashSet::new()));
        assert!(
            matches!(
                app.lifecycle.state(id),
                Some((_, Residency::Hydrating { .. }))
            ),
            "the lifecycle counts the held terminal as hydrating"
        );
        assert!(
            app.terminal_starts.lock().unwrap().is_empty(),
            "a possibly shared terminal waits for the registry"
        );

        let mut model = crate::workspace_model::WorkspaceModel::default();
        model.identity = identity;
        model.sequence = 1;
        drop(app.apply_workspace_model_update(
            super::super::workspace_model_sync::ModelUpdate::Connected(model),
        ));
        assert!(
            !app.shared_views.attached.contains(&id),
            "a terminal the registry lists in no view is this window's own"
        );
        assert!(
            app.saved_terminal_current(id).is_some(),
            "it restores as a saved terminal, as an unshared window restores it"
        );
        // The one start in flight is the saved terminal's metadata-only
        // inspection, which attaches only an exact existing terminal.
        assert_eq!(app.terminal_starts.lock().unwrap().get(&id), Some(&1));
        // Neither the lifecycle nor a later reconciliation starts the saved
        // command alongside it.
        drop(app.reconcile_lifecycle(&HashSet::from([id]), &HashSet::new(), &HashSet::new()));
        drop(app.reconcile_shared_views());
        drop(app.reconcile_lifecycle(&HashSet::from([id]), &HashSet::new(), &HashSet::new()));
        assert_eq!(
            app.terminal_starts.lock().unwrap().get(&id),
            Some(&1),
            "a saved recipe is not run without the user asking"
        );
        assert!(app.saved_terminal_current(id).is_some());
        assert!(!app.shared_views.attached.contains(&id));
    }

    /// A daemon that answers without a registry answers just the same: a held
    /// terminal must not keep waiting for a registry it has been told is not
    /// coming.
    #[test]
    fn restored_terminal_held_for_registry_restores_as_saved_once_it_is_unavailable() {
        let (mut app, _, _, _) = fixture();
        let session = fresh_terminal_session(LaunchRecipe::shell(PathBuf::from("/repo")).unwrap());
        drop(app.insert_terminal_card(session, None));
        let id = app.workspace.focused().unwrap();
        let identity = app.workspace_model.identity;
        Arc::make_mut(&mut app.durable).workspace_model_identity = Some(identity);
        app.shared_views = State::for_workspace(&app.durable);
        app.workspace_model = crate::workspace_model::WorkspaceModel::default();
        app.terminal_starts.lock().unwrap().clear();
        app.cards.insert(id, CardRuntime::Loading);
        app.set_residency(id, Residency::Hibernated);
        app.lifecycle
            .register(id, Exposure::Visible, Residency::Hibernated, Instant::now());
        drop(app.reconcile_lifecycle(&HashSet::from([id]), &HashSet::new(), &HashSet::new()));
        assert!(
            app.terminal_starts.lock().unwrap().is_empty(),
            "a possibly shared terminal waits for the registry"
        );

        drop(app.apply_workspace_model_update(
            super::super::workspace_model_sync::ModelUpdate::Unavailable(
                "daemon rejected the request (Internal): registry is unavailable".into(),
            ),
        ));
        assert!(app.saved_terminal_current(id).is_some());
        assert_eq!(app.terminal_starts.lock().unwrap().get(&id), Some(&1));
    }

    /// A terminal nobody has opened is held just the same, and the gate waits
    /// for it. It must not be reported as lost because no view lists it.
    #[test]
    fn unopened_terminal_no_view_lists_restores_as_saved_once_the_registry_answers() {
        let (mut app, id, identity) = restored_unopened_terminal();
        let mut model = crate::workspace_model::WorkspaceModel::default();
        model.identity = identity;
        model.sequence = 1;
        drop(app.apply_workspace_model_update(
            super::super::workspace_model_sync::ModelUpdate::Connected(model),
        ));
        assert!(!app.shared_views.attached.contains(&id));
        assert!(app.saved_terminal_current(id).is_some());
        assert_eq!(app.terminal_starts.lock().unwrap().get(&id), Some(&1));
        assert!(matches!(
            app.lifecycle.state(id),
            Some((_, Residency::Hibernated))
        ));
    }

    /// Opening a terminal whose daemon has no registry must not run its saved
    /// command: nothing has said no other window hosts it.
    #[test]
    fn unopened_terminal_never_runs_its_recipe_when_the_registry_is_unavailable() {
        let (mut app, id, _) = restored_unopened_terminal();
        drop(app.apply_workspace_model_update(
            super::super::workspace_model_sync::ModelUpdate::Unavailable(
                "daemon rejected the request (Internal): registry is unavailable".into(),
            ),
        ));
        assert!(app.saved_terminal_current(id).is_some());
        assert_eq!(app.terminal_starts.lock().unwrap().get(&id), Some(&1));
        let CardKind::Terminal(session) = app.card_state(id).unwrap().kind.clone() else {
            panic!("a terminal card");
        };
        drop(app.start_terminal(id, session, true, false));
        assert_eq!(
            app.terminal_starts.lock().unwrap().get(&id),
            Some(&1),
            "opening it does not start anything beyond the inspection"
        );
    }

    /// A terminal that cannot restore as saved stays attach-only while the
    /// daemon has no registry: nothing has said no other window hosts it.
    #[test]
    fn terminal_that_cannot_restore_as_saved_only_attaches_when_the_registry_is_unavailable() {
        let (mut app, id, _) = restored_unopened_terminal();
        let CardKind::Terminal(session) = &mut app.card_state_mut(id).unwrap().kind else {
            panic!("a terminal card");
        };
        session.continuity = crate::model::TerminalContinuity::Failed {
            typed_error: crate::model::RuntimeError {
                code: "failed".into(),
                message: "failed".into(),
            },
            retryable: false,
        };
        let session = session.clone();
        drop(app.apply_workspace_model_update(
            super::super::workspace_model_sync::ModelUpdate::Unavailable(
                "daemon rejected the request (Internal): registry is unavailable".into(),
            ),
        ));
        assert!(app.saved_terminal_current(id).is_none());
        assert!(app.shared_views.attached.contains(&id));
        assert!(app.terminal_start_held(id, false));
        drop(app.start_terminal(id, session, true, false));
        assert!(
            !app.terminal_starts
                .lock()
                .unwrap()
                .get(&id)
                .is_some_and(|starts| *starts > 1),
            "opening it adds no start of its own"
        );
    }

    /// A terminal restored as this window's own saved terminal can be listed
    /// in a view later, by this window's own seed or once its folder is
    /// opened as a project. Listed, it is shared and may only attach, so it
    /// must leave the saved presentation and start on the attach-only path
    /// rather than keep loading with nothing left to start it.
    #[test]
    fn saved_terminal_listed_in_a_view_later_hydrates_as_shared() {
        // The registry knows the channel, so its view can be projected here.
        let (mut app, a, _, _) = channel_move_fixture();
        let channels = app.workspace_model.channels.clone();
        let session = fresh_terminal_session(LaunchRecipe::shell(PathBuf::from("/repo")).unwrap());
        drop(app.insert_terminal_card(session, None));
        let id = app.workspace.focused().unwrap();
        // Saved before the window last closed, its creation long settled.
        Arc::make_mut(&mut app.durable)
            .pending_terminal_creations
            .remove(&id);
        let identity = app.workspace_model.identity;
        Arc::make_mut(&mut app.durable).workspace_model_identity = Some(identity);
        app.shared_views = State::for_workspace(&app.durable);
        app.workspace_model = crate::workspace_model::WorkspaceModel::default();
        app.terminal_starts.lock().unwrap().clear();
        app.cards.insert(id, CardRuntime::Loading);
        app.set_residency(id, Residency::Hibernated);
        app.lifecycle
            .register(id, Exposure::Visible, Residency::Hibernated, Instant::now());
        drop(app.reconcile_lifecycle(&HashSet::from([id]), &HashSet::new(), &HashSet::new()));
        let mut model = crate::workspace_model::WorkspaceModel::default();
        model.identity = identity;
        model.sequence = 1;
        model.channels = channels;
        drop(app.apply_workspace_model_update(
            super::super::workspace_model_sync::ModelUpdate::Connected(model),
        ));
        assert!(app.saved_terminal_current(id).is_some());
        assert_eq!(app.terminal_starts.lock().unwrap().get(&id), Some(&1));

        let mut view = View::empty(a);
        view.panes
            .push(Pane::from_card(app.card_state(id).unwrap(), None).unwrap());
        app.workspace_model.views = vec![view.clone()];
        drop(app.project_shared_view(&view, &HashSet::from([id])));
        assert!(app.shared_views.attached.contains(&id));
        assert!(
            !app.saved_terminals.entries.contains_key(&id),
            "a listed terminal is no longer presented as this window's saved terminal"
        );
        drop(app.reconcile_lifecycle(&HashSet::from([id]), &HashSet::new(), &HashSet::new()));
        assert_eq!(
            app.terminal_starts.lock().unwrap().get(&id),
            Some(&2),
            "the attach-only start runs beside the inspection it replaces"
        );
    }

    /// The shared counterpart: a held terminal the registry lists in a view
    /// this window does not project (here the fixture's channels are absent
    /// from the registry) is admitted to hydrate again and stays shared, so it
    /// only attaches, since another window may host it.
    #[test]
    fn restored_shared_terminal_held_for_registry_attaches_once_it_answers() {
        let (mut app, a, _, _) = fixture();
        let session = fresh_terminal_session(LaunchRecipe::shell(PathBuf::from("/repo")).unwrap());
        drop(app.insert_terminal_card(session, None));
        let id = app.workspace.focused().unwrap();
        let identity = app.workspace_model.identity;
        Arc::make_mut(&mut app.durable).workspace_model_identity = Some(identity);
        app.shared_views = State::for_workspace(&app.durable);
        app.shared_views.initialized.insert(Scope::Channel(a));
        app.workspace_model = crate::workspace_model::WorkspaceModel::default();
        app.terminal_starts.lock().unwrap().clear();
        app.cards.insert(id, CardRuntime::Loading);
        app.set_residency(id, Residency::Hibernated);
        app.lifecycle
            .register(id, Exposure::Visible, Residency::Hibernated, Instant::now());
        drop(app.reconcile_lifecycle(&HashSet::from([id]), &HashSet::new(), &HashSet::new()));
        assert!(app.terminal_starts.lock().unwrap().is_empty());

        let mut view = View::empty(a);
        view.panes
            .push(Pane::from_card(app.card_state(id).unwrap(), None).unwrap());
        let mut model = crate::workspace_model::WorkspaceModel::default();
        model.identity = identity;
        model.sequence = 1;
        model.views = vec![view];
        drop(app.apply_workspace_model_update(
            super::super::workspace_model_sync::ModelUpdate::Connected(model),
        ));
        drop(app.reconcile_lifecycle(&HashSet::from([id]), &HashSet::new(), &HashSet::new()));
        assert_eq!(app.terminal_starts.lock().unwrap().get(&id), Some(&1));
        assert!(app.shared_views.attached.contains(&id));
        assert!(app.saved_terminal_current(id).is_none());
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
    fn unpublished_default_chat() -> (
        Cagents,
        ChannelId,
        CardId,
        super::super::chat_ui::Target,
        async_channel::Receiver<ModelEdit>,
    ) {
        let (mut app, channel, _, _) = fixture();
        app.active_workspace = Some(PathBuf::from("/repo"));
        Arc::make_mut(&mut app.durable).active_channel = Some(channel);
        app.workspace_model.views = vec![View::empty(channel)];
        let (sender, receiver) = async_channel::unbounded();
        app.workspace_model_tx = sender;
        drop(app.add_default_pane(PathBuf::from("/repo"), settings::DefaultPane::NativeChat));
        let card = app.workspace.focused().unwrap();
        let Some(CardRuntime::Chat(runtime)) = app.cards.get(&card) else {
            panic!("native default draft");
        };
        let target = runtime.target;
        assert!(app.chat_is_unused_default(card));
        assert!(app.local_view_panes(&Scope::Channel(channel)).is_empty());
        while receiver.try_recv().is_ok() {}
        (app, channel, card, target, receiver)
    }

    fn compose_default_chat(app: &mut Cagents, target: super::super::chat_ui::Target) {
        drop(app.handle_chat_ui(super::super::chat_ui::Action::Composer(
            target,
            iced::widget::text_editor::Action::Edit(iced::widget::text_editor::Edit::Paste(
                Arc::new("first message".into()),
            )),
        )));
    }

    #[test]
    fn used_default_chat_reserves_one_insert_before_empty_view_projection() {
        let (mut app, channel, card, target, receiver) = unpublished_default_chat();
        // An untouched automatic composer remains local and ephemeral.
        drop(app.reconcile_shared_views());
        assert!(receiver.try_recv().is_err());
        compose_default_chat(&mut app, target);
        for _ in 0..3 {
            drop(app.reconcile_shared_views());
            assert!(app.card_state(card).unwrap().existence.is_open());
            assert!(app.workspace.card(card).is_some());
            assert!(!app.durable.local_only_cards.contains(&card));
            assert!(!app.shared_views.retained.contains(&card));
        }
        let edits = std::iter::from_fn(|| receiver.try_recv().ok()).collect::<Vec<_>>();
        let [(operation, pane)]: [(uuid::Uuid, Pane); 1] = edits
            .iter()
            .filter_map(|edit| match edit {
                ModelEdit::ChangeView {
                    operation,
                    change: Change::Insert { pane, .. },
                    ..
                } if pane.id == card => Some((*operation, pane.clone())),
                _ => None,
            })
            .collect::<Vec<_>>()
            .try_into()
            .unwrap();
        let mut accepted = View::empty(channel);
        accepted.revision = 1;
        accepted.panes.push(pane.clone());
        app.workspace_model.views = vec![accepted];
        app.acknowledge_view_edit(Scope::Channel(channel), Some(operation));
        drop(app.reconcile_shared_views());
        assert!(receiver.try_recv().is_err());
        assert_eq!(app.local_view_cards(&Scope::Channel(channel)), vec![card]);
        let saved = app.persistent_workspace();
        let restored = State::for_workspace(&saved);
        assert!(restored.attached.contains(&card));
        assert!(!saved.local_only_cards.contains(&card));
        assert!(
            matches!(&app.card_state(card).unwrap().kind, CardKind::Chat(session) if session.is_unstarted())
        );
    }

    fn first_start_save_completion(save: Task<Message>) -> super::super::chat_ui::Action {
        std::thread::spawn(move || {
            use cosmic::iced::futures::StreamExt;
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    let mut stream = cosmic::iced::runtime::task::into_stream(save).unwrap();
                    while let Some(action) =
                        tokio::time::timeout(Duration::from_secs(5), stream.next())
                            .await
                            .unwrap()
                    {
                        if let cosmic::iced::runtime::Action::Output(cosmic::Action::App(
                            Message::ChatUi(
                                action @ super::super::chat_ui::Action::DraftStartCheckpointed(
                                    _,
                                    _,
                                    _,
                                ),
                            ),
                        )) = action
                        {
                            return action;
                        }
                    }
                    panic!("checkpoint receipt");
                })
        })
        .join()
        .unwrap()
    }

    #[test]
    fn first_start_empty_view_does_not_convert_creation_to_local_only() {
        let (mut app, channel, card, target, receiver) = unpublished_default_chat();
        let directory = tempfile::tempdir().unwrap();
        let (writer, _) = crate::session::SessionRuntime::open(
            directory.path().join("state.sqlite3"),
            crate::model::BootId::new("first-send-view"),
        )
        .unwrap();
        app.session = Some(writer);
        compose_default_chat(&mut app, target);
        // First publish A, then Send, then an empty snapshot while the save
        // receipt is waiting. This used to project A over the new runtime B.
        drop(app.reconcile_shared_views());
        let source = match &app.card_state(card).unwrap().kind {
            CardKind::Chat(session) => session.clone(),
            _ => unreachable!(),
        };
        let save = app.handle_chat_ui(super::super::chat_ui::Action::Submit(
            target,
            crate::chat::PromptMode::Send,
        ));
        assert!(app.chat_start_pending(card));
        drop(app.reconcile_shared_views());
        assert!(!app.durable.local_only_cards.contains(&card));
        assert!(app.card_state(card).unwrap().existence.is_open());
        let completion = first_start_save_completion(save);
        assert!(matches!(
            &completion,
            super::super::chat_ui::Action::DraftStartCheckpointed(_, _, Ok(()))
        ));
        // Only the save Task ran. Drop the returned provider Task unpolled.
        let provider = app.handle_chat_ui(completion);
        let candidate = match &app.card_state(card).unwrap().kind {
            CardKind::Chat(session) => session.clone(),
            _ => unreachable!(),
        };
        assert_ne!(source.id, candidate.id);
        drop(app.reconcile_shared_views());
        assert!(
            matches!(app.cards.get(&card), Some(CardRuntime::Chat(runtime)) if runtime.session.id == candidate.id)
        );
        assert!(!app.durable.local_only_cards.contains(&card));
        assert!(
            app.local_view_cards(&Scope::Channel(channel))
                .contains(&card)
        );
        let edits = std::iter::from_fn(|| receiver.try_recv().ok()).collect::<Vec<_>>();
        assert_eq!(
            edits
                .iter()
                .filter(|edit| matches!(edit,
            ModelEdit::ChangeView { change: Change::Insert { pane, .. }, .. } if pane.id == card))
                .count(),
            1
        );
        assert!(edits.iter().any(|edit| matches!(edit,
            ModelEdit::ChangeView { change: Change::Update { before, pane }, .. }
                if pane.id == card && before.kind == CardKind::Chat(source.clone()) && pane.kind == CardKind::Chat(candidate.clone()))));
        drop(provider);
    }

    #[test]
    fn first_start_receipt_starts_a_refused_creation_locally_but_never_a_retired_one() {
        for case in ["refused", "refused-scope-gone", "retired"] {
            let (mut app, _, card, target, receiver) = unpublished_default_chat();
            let directory = tempfile::tempdir().unwrap();
            let (writer, _) = crate::session::SessionRuntime::open(
                directory.path().join("state.sqlite3"),
                crate::model::BootId::new("first-send-revoked"),
            )
            .unwrap();
            app.session = Some(writer);
            compose_default_chat(&mut app, target);
            drop(app.reconcile_shared_views());
            let operation = std::iter::from_fn(|| receiver.try_recv().ok())
                .find_map(|edit| match edit {
                    ModelEdit::ChangeView {
                        operation,
                        change: Change::Insert { pane, .. },
                        ..
                    } if pane.id == card => Some(operation),
                    _ => None,
                })
                .unwrap();
            let draft = match &app.card_state(card).unwrap().kind {
                CardKind::Chat(session) => session.id,
                _ => unreachable!(),
            };
            let save = app.handle_chat_ui(super::super::chat_ui::Action::Submit(
                target,
                crate::chat::PromptMode::Send,
            ));
            assert!(app.chat_start_pending(card));
            match case {
                "retired" => app.workspace_model.views[0].retired.push(card),
                _ => app.shared_views.refused(operation),
            }
            drop(app.reconcile_shared_views());
            assert!(app.durable.local_only_cards.contains(&card), "{case}");
            if case == "refused-scope-gone" {
                app.workspace_model.views.clear();
            }
            let completion = first_start_save_completion(save);
            assert!(matches!(
                &completion,
                super::super::chat_ui::Action::DraftStartCheckpointed(_, _, Ok(()))
            ));
            let start = app.handle_chat_ui(completion);
            let saved = app.persistent_workspace();
            assert!(saved.local_only_cards.contains(&card), "{case}");
            if case == "refused" {
                // Kept in this window, its one Send starts here and nowhere
                // else; the provider Task is dropped unpolled.
                assert!(start.units() > 0, "{case}");
                assert!(app.chat_start_pending(card), "{case}");
                assert!(
                    matches!(&app.card_state(card).unwrap().kind, CardKind::Chat(session) if session.id != draft)
                );
            } else {
                // handle_chat_ui batches even an empty result; its stream may
                // exist without containing any work. No provider future is queued.
                assert_eq!(
                    start.units(),
                    0,
                    "{case}: revoked creation must queue no provider work"
                );
                assert!(!app.chat_start_pending(card));
                assert!(
                    matches!(&app.card_state(card).unwrap().kind, CardKind::Chat(session) if session.id == draft)
                );
                assert_eq!(
                    saved.chat_drafts.get(&card).map(String::as_str),
                    Some("first message")
                );
            }
            drop(start);
            assert!(
                !std::iter::from_fn(|| receiver.try_recv().ok()).any(|edit| matches!(edit,
                    ModelEdit::ChangeView { change: Change::Insert { pane, .. }, .. } if pane.id == card)),
                "{case}: a revoked or local creation must not be recreated"
            );
        }
    }

    #[test]
    fn only_a_canonical_removal_reports_a_pane_closed_in_another_client() {
        for retired in [false, true] {
            let (mut app, channel, card, target, receiver) = unpublished_default_chat();
            compose_default_chat(&mut app, target);
            drop(app.reconcile_shared_views());
            let (operation, pane) = std::iter::from_fn(|| receiver.try_recv().ok())
                .find_map(|edit| match edit {
                    ModelEdit::ChangeView {
                        operation,
                        change: Change::Insert { pane, .. },
                        ..
                    } if pane.id == card => Some((operation, pane)),
                    _ => None,
                })
                .unwrap();
            let mut accepted = View::empty(channel);
            accepted.revision = 1;
            accepted.panes.push(pane);
            app.workspace_model.views = vec![accepted];
            app.acknowledge_view_edit(Scope::Channel(channel), Some(operation));
            drop(app.reconcile_shared_views());
            assert!(app.status.is_none());
            // Another client removes the shared pane, or a later snapshot
            // simply lacks it, while this window still has its draft.
            let view = &mut app.workspace_model.views[0];
            view.panes.clear();
            view.revision += 1;
            if retired {
                view.retired.push(card);
            }
            for read in 0..3 {
                drop(app.reconcile_shared_views());
                assert!(app.card_state(card).unwrap().existence.is_open());
                assert!(app.durable.local_only_cards.contains(&card));
                if read == 0 {
                    let notice = format!("{:?}", app.status.take().expect("one notice"));
                    assert_eq!(
                        notice.contains("closed in another client"),
                        retired,
                        "{notice}"
                    );
                } else {
                    assert!(app.status.is_none(), "the notice is not repeated");
                }
            }
        }
    }

    #[test]
    fn refused_draft_publication_is_kept_without_claiming_another_client_closed_it() {
        let (mut app, channel, card, target, receiver) = unpublished_default_chat();
        compose_default_chat(&mut app, target);
        drop(app.reconcile_shared_views());
        let operation = std::iter::from_fn(|| receiver.try_recv().ok())
            .find_map(|edit| match edit {
                ModelEdit::ChangeView {
                    operation,
                    change: Change::Insert { pane, .. },
                    ..
                } if pane.id == card => Some(operation),
                _ => None,
            })
            .unwrap();
        app.refuse_shared_view_edit(&Scope::Channel(channel), operation);
        assert!(app.durable.local_only_cards.contains(&card));
        assert!(
            super::super::chat_ui::chat_error_for_test(&app, card)
                .is_some_and(|error| error.contains("could not be shared"))
        );
        for _ in 0..3 {
            drop(app.reconcile_shared_views());
            assert!(app.card_state(card).unwrap().existence.is_open());
            assert_eq!(app.workspace.focused(), Some(card));
            assert!(app.status.is_none(), "nothing was closed in another client");
        }
        assert!(receiver.try_recv().is_err(), "a refusal is not republished");
        assert_eq!(
            app.persistent_workspace()
                .chat_drafts
                .get(&card)
                .map(String::as_str),
            Some("first message")
        );
        // The kept draft still sends, in this window only.
        let directory = tempfile::tempdir().unwrap();
        let (writer, _) = crate::session::SessionRuntime::open(
            directory.path().join("state.sqlite3"),
            crate::model::BootId::new("refused-draft-send"),
        )
        .unwrap();
        app.session = Some(writer);
        let save = app.handle_chat_ui(super::super::chat_ui::Action::Submit(
            target,
            crate::chat::PromptMode::Send,
        ));
        let start = app.handle_chat_ui(first_start_save_completion(save));
        assert!(start.units() > 0);
        assert!(app.chat_start_pending(card));
        drop(start);
        drop(app.reconcile_shared_views());
        assert!(app.card_state(card).unwrap().existence.is_open());
        assert!(app.durable.local_only_cards.contains(&card));
        assert!(
            !std::iter::from_fn(|| receiver.try_recv().ok()).any(|edit| matches!(edit,
                ModelEdit::ChangeView { change: Change::Insert { pane, .. }, .. } if pane.id == card))
        );
    }

    #[test]
    fn transferred_native_draft_can_complete_its_first_start_checkpoint() {
        let (mut app, channel, card, target, receiver) = unpublished_default_chat();
        compose_default_chat(&mut app, target);
        drop(app.reconcile_shared_views());
        let pane = app
            .local_view_panes(&Scope::Channel(channel))
            .into_iter()
            .find(|pane| pane.id == card)
            .unwrap();
        let source = app
            .durable
            .channels
            .iter()
            .find(|entry| entry.id != channel)
            .unwrap()
            .id;
        let mut retired_source = View::empty(source);
        retired_source.retired.push(card);
        app.workspace_model.views[0].panes.push(pane);
        app.workspace_model.views.push(retired_source);
        app.shared_views.pending.clear();
        drop(app.reconcile_shared_views());
        while receiver.try_recv().is_ok() {}
        let directory = tempfile::tempdir().unwrap();
        let (writer, _) = crate::session::SessionRuntime::open(
            directory.path().join("state.sqlite3"),
            crate::model::BootId::new("first-send-transferred"),
        )
        .unwrap();
        app.session = Some(writer);
        let save = app.handle_chat_ui(super::super::chat_ui::Action::Submit(
            target,
            crate::chat::PromptMode::Send,
        ));
        let completion = first_start_save_completion(save);
        assert!(matches!(
            &completion,
            super::super::chat_ui::Action::DraftStartCheckpointed(_, _, Ok(()))
        ));
        let start = app.handle_chat_ui(completion);
        assert!(app.chat_start_pending(card));
        assert!(start.units() > 0);
        drop(start);
        assert!(!app.durable.local_only_cards.contains(&card));
        assert_eq!(app.local_view_cards(&Scope::Channel(channel)), vec![card]);
    }

    #[test]
    fn refused_default_chat_insert_does_not_republish_on_empty_reads() {
        let (mut app, _, card, target, receiver) = unpublished_default_chat();
        compose_default_chat(&mut app, target);
        drop(app.reconcile_shared_views());
        let operation = std::iter::from_fn(|| receiver.try_recv().ok())
            .find_map(|edit| match edit {
                ModelEdit::ChangeView {
                    operation,
                    change: Change::Insert { pane, .. },
                    ..
                } if pane.id == card => Some(operation),
                _ => None,
            })
            .unwrap();
        app.shared_views.refused(operation);
        for _ in 0..3 {
            drop(app.reconcile_shared_views());
        }
        assert!(app.durable.local_only_cards.contains(&card));
        assert!(
            receiver.try_recv().is_err(),
            "a refusal grants no creation retry"
        );
    }

    #[test]
    fn retired_default_chat_does_not_gain_creation_authority() {
        let (mut app, _, card, target, receiver) = unpublished_default_chat();
        compose_default_chat(&mut app, target);
        app.workspace_model.views[0].retired.push(card);
        drop(app.reconcile_shared_views());
        assert!(app.durable.local_only_cards.contains(&card));
        assert!(receiver.try_recv().is_err());
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

    #[test]
    fn channel_move_waits_for_actual_terminal_start_completion() {
        let (mut app, source, destination, mut pane) = channel_move_fixture();
        pane.kind = moving_terminal_kind();
        let CardKind::Terminal(session) = &pane.kind else {
            unreachable!();
        };
        app.workspace_model
            .views
            .iter_mut()
            .flat_map(|view| &mut view.panes)
            .find(|current| current.id == pane.id)
            .unwrap()
            .kind = pane.kind.clone();
        // Projection registers the actual start before its worker Task runs.
        // Hold that Task and exercise the ordinary menu/direct/drop handlers.
        let attach = app.reconcile_shared_views();
        assert!(app.terminal_start_in_flight(pane.id));
        let (tx, rx) = async_channel::unbounded();
        app.workspace_model_tx = tx;
        assert_lifecycle_refuses_moves(&mut app, &rx, source, destination, pane.id);
        drop(attach);
        drop(app.update(Message::TerminalStarted {
            card: pane.id,
            terminal: session.id,
            result: Err("fixture holds the real terminal attach".into()),
        }));
        assert!(!app.terminal_start_in_flight(pane.id));
        assert_eq!(app.card_state(pane.id).unwrap().kind, pane.kind);
        assert!(app.can_move_pane_to_channel(pane.id, destination));
        drop(app.move_pane_to_channel(pane.id, destination));
        assert!(matches!(rx.try_recv().unwrap(), ModelEdit::ChangeView {
            change: Change::Transfer { id, .. }, ..
        } if id == pane.id));
    }

    fn assert_lifecycle_refuses_moves(
        app: &mut Cagents,
        rx: &async_channel::Receiver<ModelEdit>,
        source: ChannelId,
        destination: ChannelId,
        card: CardId,
    ) {
        let before = app.workspace_model.clone();
        assert!(!app.pane_has_move_destination(card));
        assert!(!app.can_move_pane_to_channel(card, destination));
        assert!(app.pane_move_destinations(card).is_empty());
        drop(app.move_pane_to_channel(card, destination));
        drop(app.update(Message::MovePaneToChannel {
            card,
            channel: destination,
        }));
        drop(app.update(Message::PaneDropToChannel {
            generation: app.workspace_view_generation,
            card,
            channel: destination,
        }));
        drop(app.update_palette(super::super::palette_ui::Action::MovePane(card)));
        assert!(!app.palette.is_open());
        assert!(rx.try_recv().is_err());
        assert!(!app.pane_transfer_pending(card));
        assert_eq!(app.workspace_model, before);
        assert_eq!(app.durable.active_channel, Some(source));
        assert!(app.card_state(card).unwrap().existence.is_open());
    }

    #[test]
    fn channel_move_waits_for_an_existing_workspace_close_handoff() {
        let (mut app, source, destination, pane) = channel_move_fixture();
        let (tx, rx) = async_channel::unbounded();
        app.workspace_model_tx = tx;
        assert!(app.can_move_pane_to_channel(pane.id, destination));
        // Hold the real close's focus task before it can commit removal.
        let close = app.begin_card_close(pane.id);
        assert!(
            app.workspace
                .pending_close()
                .is_some_and(|handoff| handoff.unmounting == pane.id)
        );
        assert_lifecycle_refuses_moves(&mut app, &rx, source, destination, pane.id);
        drop(close);
        app.workspace.cancel_close(pane.id).unwrap();
        assert!(app.can_move_pane_to_channel(pane.id, destination));
        drop(app.move_pane_to_channel(pane.id, destination));
        assert!(matches!(rx.try_recv().unwrap(), ModelEdit::ChangeView {
            change: Change::Transfer { id, .. }, ..
        } if id == pane.id));
    }

    #[test]
    fn channel_move_waits_for_pending_terminal_cleanup_and_explicit_dismissal() {
        let (mut app, source, destination, pane) =
            channel_move_fixture_with_kind(Some(moving_terminal_kind()));
        let (tx, rx) = async_channel::unbounded();
        app.workspace_model_tx = tx;
        assert!(app.can_move_pane_to_channel(pane.id, destination));
        // Begin the actual production close, but hold its worker task unpolled.
        let close = app.begin_card_close(pane.id);
        let intent = app.terminal_close.as_ref().unwrap().clone();
        assert!(intent.pending);
        assert!(!app.modal_is_open());
        assert_lifecycle_refuses_moves(&mut app, &rx, source, destination, pane.id);
        drop(close);
        drop(app.terminal_close_finished(
            pane.id,
            intent.terminal,
            intent.proof,
            intent.generation,
            super::super::terminal_close::CloseOutcome::Unreachable("fixture held close".into()),
        ));
        assert!(!app.can_move_pane_to_channel(pane.id, destination));
        drop(app.cancel_terminal_close());
        assert!(app.can_move_pane_to_channel(pane.id, destination));
        drop(app.move_pane_to_channel(pane.id, destination));
        assert!(matches!(rx.try_recv().unwrap(), ModelEdit::ChangeView {
            change: Change::Transfer { id, .. }, ..
        } if id == pane.id));
    }

    #[test]
    fn channel_move_stays_available_while_an_unrelated_terminal_is_closing() {
        let (mut app, source, destination, pane) = channel_move_fixture();
        let mut other = pane.clone();
        other.id = CardId::new();
        other.kind = moving_terminal_kind();
        app.workspace_model
            .views
            .iter_mut()
            .find(|view| view.scope == Scope::Channel(source))
            .unwrap()
            .panes
            .push(other.clone());
        drop(app.reconcile_shared_views());
        let (tx, rx) = async_channel::unbounded();
        app.workspace_model_tx = tx;
        let close = app.begin_card_close(other.id);
        assert!(app.terminal_close_pending(other.id));
        assert!(!app.modal_is_open());
        assert!(!app.can_move_pane_to_channel(other.id, destination));
        assert!(app.can_move_pane_to_channel(pane.id, destination));
        drop(app.move_pane_to_channel(pane.id, destination));
        assert!(matches!(rx.try_recv().unwrap(), ModelEdit::ChangeView {
            change: Change::Transfer { id, .. }, ..
        } if id == pane.id));
        assert!(rx.try_recv().is_err());
        assert_eq!(app.terminal_close.as_ref().unwrap().card, other.id);
        assert!(app.card_state(other.id).unwrap().existence.is_open());
        drop(close);
    }

    fn retained_chat_move_fixture() -> (Cagents, ChannelId, ChannelId, Pane) {
        use crate::chat;
        let (mut app, source, destination, mut pane) = channel_move_fixture();
        let CardKind::Terminal(terminal) = moving_terminal_kind() else {
            unreachable!()
        };
        let session = chat::Session {
            id: TerminalId::new(),
            proof: terminal.continuity.live_proof(),
            terminal: None,
            owned_codex_listener: None,
            native_continuity: chat::NativeContinuity::Durable {
                provider_version: "fixture".into(),
            },
            launch: chat::Launch {
                provider: crate::agent::Provider::Claude,
                recipe: terminal.launch,
                session_id: Some("retained-close-fixture".into()),
            },
        };
        let kind = CardKind::Chat(session);
        pane.kind = kind.clone();
        app.card_state_mut(pane.id).unwrap().kind = kind.clone();
        app.workspace_model
            .views
            .iter_mut()
            .flat_map(|view| &mut view.panes)
            .find(|candidate| candidate.id == pane.id)
            .unwrap()
            .kind = kind;
        // This is the real recovery path: the saved owner remains while its
        // unavailable surface waits for an exact native close receipt.
        app.cards.insert(
            pane.id,
            CardRuntime::Failed("fixture detached native surface".into()),
        );
        (app, source, destination, pane)
    }

    #[test]
    fn channel_move_waits_for_retained_native_chat_cleanup_without_a_modal() {
        let (mut app, source, destination, pane) = retained_chat_move_fixture();
        let (tx, rx) = async_channel::unbounded();
        app.workspace_model_tx = tx;
        assert!(app.can_move_pane_to_channel(pane.id, destination));
        let close = app.begin_chat_close(pane.id);
        assert!(app.chat_start_pending(pane.id));
        assert!(!app.modal_is_open());
        assert_lifecycle_refuses_moves(&mut app, &rx, source, destination, pane.id);
        drop(close);
    }

    #[test]
    fn pending_channel_move_refuses_native_close_and_switch_until_refused() {
        let (mut app, source, destination, pane) = retained_chat_move_fixture();
        let (tx, rx) = async_channel::unbounded();
        app.workspace_model_tx = tx;
        let before = app.workspace_model.clone();
        drop(app.move_pane_to_channel(pane.id, destination));
        let ModelEdit::ChangeView {
            operation,
            change: Change::Transfer { .. },
            ..
        } = rx.try_recv().unwrap()
        else {
            panic!("transfer request")
        };
        assert!(app.pane_transfer_pending(pane.id));
        assert!(!app.chat_start_pending(pane.id));
        assert!(app.chat_switch_blocker(pane.id).is_some());
        drop(app.begin_chat_close(pane.id));
        drop(app.update(Message::CloseCard(pane.id)));
        drop(app.handle_chat_ui(super::super::chat_ui::Action::Switch(pane.id)));
        assert!(!app.chat_start_pending(pane.id));
        assert_eq!(app.card_state(pane.id).unwrap().kind, pane.kind);
        assert!(app.card_state(pane.id).unwrap().existence.is_open());
        assert_eq!(app.durable.active_channel, Some(source));
        assert_eq!(app.workspace_model, before);
        assert!(rx.try_recv().is_err());
        app.reject_pane_transfer(operation, "fixture rejected move".into());
        assert!(!app.pane_transfer_pending(pane.id));
        assert!(app.chat_switch_blocker(pane.id).is_none());
        let close = app.begin_chat_close(pane.id);
        assert!(app.chat_start_pending(pane.id));
        drop(close);
    }

    #[test]
    fn pending_channel_move_refuses_bulk_channel_delete_before_close_capture() {
        let (mut app, source, destination, pane) =
            channel_move_fixture_with_kind(Some(moving_terminal_kind()));
        let (tx, rx) = async_channel::unbounded();
        app.workspace_model_tx = tx;
        drop(app.move_pane_to_channel(pane.id, destination));
        let ModelEdit::ChangeView {
            operation,
            change: Change::Transfer { .. },
            ..
        } = rx.try_recv().unwrap()
        else {
            panic!("transfer request");
        };
        let close = app.delete_channel(source);
        assert!(!app.channel_ui.is_deleting(source));
        assert!(app.pane_transfer_pending(pane.id));
        assert_eq!(app.durable.active_channel, Some(source));
        assert!(app.card_state(pane.id).unwrap().existence.is_open());
        assert!(
            rx.try_recv().is_err(),
            "delete cannot capture or publish while the move owns its pane"
        );
        drop(close);
        app.reject_pane_transfer(operation, "fixture refusal".into());
        let close = app.delete_channel(source);
        assert!(
            app.channel_ui.is_deleting(source),
            "ordinary deletion is eligible after refusal"
        );
        assert!(!app.channel_ui.close_operation(source).is_nil());
        drop(close); // Keep the real production close task unpolled: no daemon effects.
    }

    #[test]
    fn held_bulk_channel_delete_refuses_moves_from_its_source_only() {
        let (mut app, source, destination, pane) =
            channel_move_fixture_with_kind(Some(moving_terminal_kind()));
        let (tx, rx) = async_channel::unbounded();
        app.workspace_model_tx = tx;
        let close = app.delete_channel(source);
        assert!(app.channel_ui.is_deleting(source));
        while rx.try_recv().is_ok() {}
        assert!(!app.pane_has_move_destination(pane.id));
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
        assert!(!app.pane_transfer_pending(pane.id));
        assert!(
            !std::iter::from_fn(|| rx.try_recv().ok()).any(|edit| matches!(
                edit,
                ModelEdit::ChangeView {
                    change: Change::Transfer { .. },
                    ..
                }
            ))
        );
        let operation = app.channel_ui.close_operation(source);
        drop(
            app.handle_channel_ui(super::super::channel_ui::Action::SessionsClosed(
                source,
                operation,
                Err("fixture cleanup refused".into()),
            )),
        );
        assert!(
            app.can_move_pane_to_channel(pane.id, destination),
            "failed deletion releases only its local ownership"
        );
        drop(close);
    }

    #[test]
    fn delayed_channel_close_never_retires_moved_replaced_or_new_members() {
        for changed in [
            "moved",
            "replacement",
            "model replacement",
            "missing shared pane",
            "new member",
            "replacement registry",
            "reopened member",
            "previously missing member",
        ] {
            let (mut app, source, destination, pane) =
                channel_move_fixture_with_kind(Some(moving_terminal_kind()));
            let (tx, rx) = async_channel::unbounded();
            app.workspace_model_tx = tx;
            let inactive = CardId::new();
            if matches!(changed, "reopened member" | "previously missing member") {
                Arc::make_mut(&mut app.durable)
                    .channels
                    .iter_mut()
                    .find(|channel| channel.id == source)
                    .unwrap()
                    .cards
                    .push(inactive);
                if changed == "reopened member" {
                    let mut closed = app.card_state(pane.id).unwrap().clone();
                    closed.id = inactive;
                    closed.existence =
                        super::super::project_ui::removed_existence("fixture already closed");
                    Arc::make_mut(&mut app.durable).cards.push(closed);
                }
            }
            let close = app.delete_channel(source);
            let operation = app.channel_ui.close_operation(source);
            while rx.try_recv().is_ok() {}
            match changed {
                "moved" => {
                    views::transfer(
                        &mut app.workspace_model.views,
                        &Scope::Channel(source),
                        pane.id,
                        &Scope::Channel(destination),
                        None,
                    )
                    .unwrap();
                    drop(app.reconcile_shared_views());
                }
                "replacement" | "model replacement" => {
                    let CardKind::Terminal(mut successor) = pane.kind.clone() else {
                        unreachable!()
                    };
                    successor.id = TerminalId::new();
                    successor.continuity = TerminalContinuity::Live {
                        host_session_id: crate::model::HostSessionId::new("successor-host"),
                        boot_id: crate::model::BootId::new("move-boot"),
                        host_incarnation: crate::model::HostIncarnation::new(
                            "successor-incarnation",
                        ),
                    };
                    if changed == "replacement" {
                        app.card_state_mut(pane.id).unwrap().kind = CardKind::Terminal(successor);
                    } else {
                        app.workspace_model
                            .views
                            .iter_mut()
                            .flat_map(|view| &mut view.panes)
                            .find(|candidate| candidate.id == pane.id)
                            .unwrap()
                            .kind = CardKind::Terminal(successor);
                    }
                }
                "replacement registry" => {
                    let mut replacement = app.workspace_model.clone();
                    replacement.identity = uuid::Uuid::new_v4();
                    let identity = replacement.identity;
                    drop(app.apply_workspace_model_update(
                        super::super::workspace_model_sync::ModelUpdate::Connected(replacement),
                    ));
                    assert_eq!(app.workspace_model.identity, identity);
                    assert_eq!(app.card_state(pane.id).unwrap().kind, pane.kind);
                }
                "reopened member" => {
                    app.card_state_mut(inactive).unwrap().existence = Existence::Open
                }
                "previously missing member" => {
                    let mut reopened = app.card_state(pane.id).unwrap().clone();
                    reopened.id = inactive;
                    Arc::make_mut(&mut app.durable).cards.push(reopened);
                }
                "missing shared pane" => app
                    .workspace_model
                    .views
                    .iter_mut()
                    .for_each(|view| view.panes.retain(|candidate| candidate.id != pane.id)),
                "new member" => Arc::make_mut(&mut app.durable)
                    .channels
                    .iter_mut()
                    .find(|channel| channel.id == source)
                    .unwrap()
                    .cards
                    .push(CardId::new()),
                _ => unreachable!(),
            }
            drop(
                app.handle_channel_ui(super::super::channel_ui::Action::SessionsClosed(
                    source,
                    operation,
                    Ok(()),
                )),
            );
            assert!(
                app.durable
                    .channels
                    .iter()
                    .any(|channel| channel.id == source),
                "{changed}"
            );
            assert!(
                app.card_state(pane.id).unwrap().existence.is_open(),
                "{changed}"
            );
            assert!(!app.channel_ui.is_deleting(source));
            assert!(
                !std::iter::from_fn(|| rx.try_recv().ok())
                    .any(|edit| matches!(edit, ModelEdit::DeleteChannel { .. })),
                "{changed}"
            );
            drop(close);
        }
    }

    #[test]
    fn retained_channel_close_accepts_its_projected_retirement_before_the_receipt() {
        for watcher_first in [false, true] {
            for sibling in [false, true] {
                let (mut app, source, _, pane) = retained_chat_move_fixture();
                if sibling {
                    let mut other = pane.clone();
                    other.id = CardId::new();
                    other.kind = CardKind::Browser(BrowserSession {
                        id: BrowserId::new(),
                        current_url: "https://example.com".parse().unwrap(),
                        history: vec![],
                        history_index: 0,
                        scroll_x: 0.,
                        scroll_y: 0.,
                        fidelity: BrowserRestoreFidelity::UrlOnly,
                    });
                    app.workspace_model.views[0].panes.push(other);
                    drop(app.reconcile_shared_views());
                }
                let (tx, rx) = async_channel::unbounded();
                app.workspace_model_tx = tx;
                let close = app.delete_channel(source);
                let operation = app.channel_ui.close_operation(source);
                assert!(!operation.is_nil());
                while rx.try_recv().is_ok() {}

                if watcher_first {
                    let view = app
                        .workspace_model
                        .views
                        .iter_mut()
                        .find(|view| view.scope == Scope::Channel(source))
                        .unwrap();
                    view.apply(&Change::Remove { id: pane.id }).unwrap();
                    view.revision += 1;
                    drop(app.reconcile_shared_views());
                    assert!(!app.card_state(pane.id).unwrap().existence.is_open());
                    assert_eq!(app.card_state(pane.id).unwrap().kind, pane.kind);
                    let channel = app
                        .durable
                        .channels
                        .iter()
                        .find(|channel| channel.id == source)
                        .unwrap();
                    assert!(!channel.cards.contains(&pane.id));
                    assert_eq!(channel.cards.len(), usize::from(sibling));
                    assert!(app.channel_ui.is_deleting(source));
                }

                drop(
                    app.handle_channel_ui(super::super::channel_ui::Action::SessionsClosed(
                        source,
                        operation,
                        Ok(()),
                    )),
                );
                if !watcher_first {
                    let view = app
                        .workspace_model
                        .views
                        .iter_mut()
                        .find(|view| view.scope == Scope::Channel(source))
                        .unwrap();
                    view.apply(&Change::Remove { id: pane.id }).unwrap();
                    view.revision += 1;
                    drop(app.reconcile_shared_views());
                }
                assert!(
                    !app.durable
                        .channels
                        .iter()
                        .any(|channel| channel.id == source),
                    "watcher_first={watcher_first}, sibling={sibling}"
                );
                assert!(!app.channel_ui.is_deleting(source));
                assert!(!app.card_state(pane.id).unwrap().existence.is_open());
                assert_eq!(
                    std::iter::from_fn(|| rx.try_recv().ok())
                        .filter(
                            |edit| matches!(edit, ModelEdit::DeleteChannel { id } if *id == source)
                        )
                        .count(),
                    1,
                    "one successful close deletes its channel once"
                );
                // The registry deletes this view with the channel. A close
                // whose projection beat its receipt must still have a known
                // deleted origin after that last canonical record is gone.
                app.workspace_model
                    .views
                    .retain(|view| view.scope != Scope::Channel(source));
                assert_eq!(
                    app.closed_card_home(pane.id),
                    Some(super::super::channel_ui::ClosedCardHome::Strip),
                    "watcher_first={watcher_first}, sibling={sibling}"
                );
                drop(close); // No provider request is polled by this ordering fixture.
            }
        }
    }

    #[test]
    fn retained_channel_close_requires_source_retirement_without_replacement_or_new_members() {
        for changed in [
            "missing retirement",
            "wrong retirement scope",
            "replacement",
            "new member",
            "moved",
        ] {
            let (mut app, source, destination, pane) = retained_chat_move_fixture();
            let (tx, rx) = async_channel::unbounded();
            app.workspace_model_tx = tx;
            let close = app.delete_channel(source);
            let operation = app.channel_ui.close_operation(source);
            while rx.try_recv().is_ok() {}

            if changed == "moved" {
                views::transfer(
                    &mut app.workspace_model.views,
                    &Scope::Channel(source),
                    pane.id,
                    &Scope::Channel(destination),
                    None,
                )
                .unwrap();
            } else {
                let view = app
                    .workspace_model
                    .views
                    .iter_mut()
                    .find(|view| view.scope == Scope::Channel(source))
                    .unwrap();
                view.apply(&Change::Remove { id: pane.id }).unwrap();
                view.revision += 1;
                if matches!(changed, "missing retirement" | "wrong retirement scope") {
                    view.retired.retain(|id| *id != pane.id);
                }
                if changed == "wrong retirement scope" {
                    app.workspace_model
                        .views
                        .iter_mut()
                        .find(|view| view.scope == Scope::Channel(destination))
                        .unwrap()
                        .retired
                        .push(pane.id);
                }
            }
            drop(app.reconcile_shared_views());
            if changed == "replacement" {
                let CardKind::Chat(session) = &mut app.card_state_mut(pane.id).unwrap().kind else {
                    unreachable!()
                };
                session.id = TerminalId::new();
            }
            let new_member = (changed == "new member").then(CardId::new);
            if let Some(id) = new_member {
                Arc::make_mut(&mut app.durable)
                    .channels
                    .iter_mut()
                    .find(|channel| channel.id == source)
                    .unwrap()
                    .cards
                    .push(id);
            }
            let current = app.card_state(pane.id).unwrap().clone();
            drop(
                app.handle_channel_ui(super::super::channel_ui::Action::SessionsClosed(
                    source,
                    operation,
                    Ok(()),
                )),
            );
            let channel = app
                .durable
                .channels
                .iter()
                .find(|channel| channel.id == source)
                .unwrap_or_else(|| panic!("{changed}: stale close deleted its channel"));
            if let Some(id) = new_member {
                assert!(channel.cards.contains(&id));
            }
            assert_eq!(app.card_state(pane.id).unwrap(), &current, "{changed}");
            assert!(!app.channel_ui.is_deleting(source));
            assert!(
                !std::iter::from_fn(|| rx.try_recv().ok())
                    .any(|edit| matches!(edit, ModelEdit::DeleteChannel { .. })),
                "{changed}"
            );
            drop(close);
        }
    }

    #[test]
    fn channel_close_completion_is_attempt_bound_and_accepts_ordinary_terminal_exit() {
        let (mut app, source, _, pane) =
            channel_move_fixture_with_kind(Some(moving_terminal_kind()));
        let first = app.delete_channel(source);
        let old = app.channel_ui.close_operation(source);
        drop(
            app.handle_channel_ui(super::super::channel_ui::Action::SessionsClosed(
                source,
                old,
                Err("fixture retry".into()),
            )),
        );
        let second = app.handle_channel_ui(super::super::channel_ui::Action::RetryFailed(source));
        let current = app.channel_ui.close_operation(source);
        assert_ne!(old, current);
        drop(
            app.handle_channel_ui(super::super::channel_ui::Action::SessionsClosed(
                source,
                old,
                Ok(()),
            )),
        );
        assert!(
            app.channel_ui.is_deleting(source),
            "a late old completion cannot consume the new attempt"
        );
        assert_eq!(app.channel_ui.close_operation(source), current);
        app.retire_ended_terminal(pane.id, 0, None, true);
        assert!(
            terminal_ended(match &app.card_state(pane.id).unwrap().kind {
                CardKind::Terminal(session) => session,
                _ => unreachable!(),
            }),
            "the actual terminal event reducer produced its normal ended descriptor"
        );
        drop(
            app.handle_channel_ui(super::super::channel_ui::Action::SessionsClosed(
                source,
                current,
                Ok(()),
            )),
        );
        assert!(
            !app.durable
                .channels
                .iter()
                .any(|channel| channel.id == source)
        );
        assert!(!app.card_state(pane.id).unwrap().existence.is_open());
        drop((first, second));
    }

    /// Unsent composer text belongs to the card. Another client replacing an
    /// unstarted chat's identity, then moving the card to another channel, must
    /// leave the text in the composer and in what autosave writes. That holds
    /// when the replacement is already started, as when another client's Send
    /// lands before this window sees its unstarted candidate.
    #[test]
    fn unsent_text_survives_replaced_identity_and_transfer() {
        use crate::app::workspace_model_sync::ModelUpdate;
        const TEXT: &str = "unsent fixture draft\n  keeps its spacing ";
        for (replace, transfer, started) in [
            (true, false, false),
            (true, true, false),
            (false, true, false),
            (true, false, true),
        ] {
            let case = format!("replace={replace} transfer={transfer} started={started}");
            let (mut app, a, b, _) = fixture();
            app.active_workspace = Some(PathBuf::from("/repo"));
            Arc::make_mut(&mut app.durable).active_channel = Some(a);
            drop(app.new_native_chat(crate::agent::Provider::Claude, Some("/repo".into())));
            let card = app.workspace.focused().unwrap();
            let target = match app.cards.get(&card) {
                Some(CardRuntime::Chat(runtime)) => runtime.target,
                _ => panic!("{case}: a new chat opens a draft runtime"),
            };
            drop(app.update(Message::ChatUi(chat_ui::Action::Composer(
                target,
                cosmic::widget::text_editor::Action::Edit(
                    cosmic::widget::text_editor::Edit::Paste(Arc::new(TEXT.into())),
                ),
            ))));
            let mut pane = Pane::from_card(app.card_state(card).unwrap(), None).unwrap();
            app.shared_views.pending.clear();
            let mut model = app.workspace_model.clone();
            model.views = vec![View::empty(a), View::empty(b)];
            model.channels = [a, b]
                .into_iter()
                .map(|id| crate::workspace_model::ModelChannel {
                    id,
                    repository: PathBuf::from("/repo"),
                    title: "view".into(),
                    title_source: Default::default(),
                    ordinal: 1,
                })
                .collect();
            model.views[0].panes.push(pane.clone());
            let mut apply =
                |app: &mut Cagents, model: &mut crate::workspace_model::WorkspaceModel, scope| {
                    model.sequence += 1;
                    for view in &mut model.views {
                        view.revision += 1;
                    }
                    drop(app.apply_workspace_model_update(ModelUpdate::ViewApplied {
                        scope,
                        operation: None,
                        model: model.clone(),
                    }));
                };
            apply(&mut app, &mut model, Scope::Channel(a));
            if replace {
                let CardKind::Chat(session) = &mut pane.kind else {
                    unreachable!()
                };
                session.id = TerminalId::new();
                if started {
                    session.launch.session_id = Some("fixture-started-conversation".into());
                }
                assert_eq!(session.is_unstarted(), !started, "{case}");
                model.views[0].panes = vec![pane.clone()];
                apply(&mut app, &mut model, Scope::Channel(a));
            }
            if started {
                // The started conversation attaches asynchronously and opens
                // its composer with the text it was handed.
                assert!(app.card_state(card).unwrap().existence.is_open(), "{case}");
                let ready = chat_ui::ready_message_for_pending_switch(
                    &app,
                    card,
                    crate::chat::Snapshot::default(),
                );
                drop(app.update(ready));
                match app.cards.get(&card) {
                    Some(CardRuntime::Chat(runtime)) => {
                        assert!(!runtime.session.is_unstarted(), "{case}: attached");
                        assert_eq!(runtime.unsent_text(), TEXT, "{case}: composer");
                    }
                    _ => panic!("{case}: the started conversation attaches"),
                }
                assert_eq!(
                    app.persistent_workspace()
                        .chat_drafts
                        .get(&card)
                        .map(String::as_str),
                    Some(TEXT),
                    "{case}: autosave"
                );
                continue;
            }
            if transfer {
                model.views[0].panes.clear();
                model.views[1].panes = vec![pane.clone()];
                apply(&mut app, &mut model, Scope::Channel(b));
                // A repeated delivery must not open the draft a second time
                // without its text.
                apply(&mut app, &mut model, Scope::Channel(b));
            }
            let home = if transfer { b } else { a };
            assert!(
                app.durable
                    .channels
                    .iter()
                    .find(|channel| channel.id == home)
                    .is_some_and(|channel| channel.cards == [card]),
                "{case}: the card follows the view"
            );
            assert!(app.card_state(card).unwrap().existence.is_open(), "{case}");
            match app.cards.get(&card) {
                Some(CardRuntime::Chat(runtime)) => {
                    assert_eq!(runtime.unsent_text(), TEXT, "{case}: composer");
                    assert_eq!(
                        Some(&runtime.session.id),
                        match &pane.kind {
                            CardKind::Chat(session) => Some(&session.id),
                            _ => None,
                        },
                        "{case}: the runtime follows the canonical identity"
                    );
                }
                _ => panic!("{case}: the card keeps a chat runtime"),
            }
            assert_eq!(
                app.persistent_workspace()
                    .chat_drafts
                    .get(&card)
                    .map(String::as_str),
                Some(TEXT),
                "{case}: autosave"
            );
        }
    }
}
