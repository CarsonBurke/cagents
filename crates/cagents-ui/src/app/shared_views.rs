//! A client projection of daemon-owned scope views. Pending local intentions
//! are layered over snapshots until acknowledged; snapshots never erase them.
mod discovery;
pub(super) mod seed_refusal;
mod settlement;
mod view_index;

use super::*;
use crate::workspace_model::{
    ModelEdit,
    views::{self, Change, Pane, Scope, View},
};
mod removal;
pub(super) use removal::{TerminalCleanupAuthority, TerminalRemovalReceipt};

/// One immutable projection for a publication pass. Scope discovery includes
/// historical directories, but their cards never become open shared panes.
struct LocalViews {
    scopes: Vec<Scope>,
    cards: HashMap<Scope, Vec<CardId>>,
    panes: HashMap<Scope, Vec<Pane>>,
    /// Seeded scopes whose panes equal their baseline, so `panes` leaves them
    /// out: publishing one changes nothing unless a transfer reaches it. Only
    /// a complete save's projection settles any.
    settled: HashSet<Scope>,
}

/// One pane of a scope as this window would publish it, borrowed from its
/// card and what is layered over it, so a pane that matches its baseline is
/// never copied.
enum LocalPane<'a> {
    /// A terminal whose start has not saved, held as its view lists it.
    Held(&'a Pane),
    Card {
        card: &'a CardState,
        title: &'a str,
        user_title: Option<&'a String>,
        pending_terminal_start: bool,
        scope: Option<&'a Path>,
    },
}

impl<'p> LocalPane<'p> {
    fn to_pane(&self) -> Pane {
        match *self {
            Self::Held(pane) => pane.clone(),
            Self::Card {
                card,
                title,
                user_title,
                pending_terminal_start,
                scope,
            } => Pane {
                id: card.id,
                title: title.to_owned(),
                user_title: user_title.cloned(),
                pending_terminal_start,
                width_fraction: card.width_fraction,
                kind: views::shared_kind(&card.kind),
                scope: scope.map(Path::to_owned),
            },
        }
    }

    /// Whether `local` would publish exactly `shared`, in order.
    fn all_match(mut local: impl Iterator<Item = LocalPane<'p>>, shared: &[Pane]) -> bool {
        let mut shared = shared.iter();
        loop {
            match (local.next(), shared.next()) {
                (None, None) => return true,
                (Some(local), Some(shared)) if local.matches(shared) => {}
                _ => return false,
            }
        }
    }

    /// Whether [`Self::to_pane`] would equal `pane`.
    fn matches(&self, pane: &Pane) -> bool {
        match *self {
            Self::Held(held) => held == pane,
            Self::Card {
                card,
                title,
                user_title,
                pending_terminal_start,
                scope,
            } => {
                let Pane {
                    id: shared_id,
                    title: shared_title,
                    user_title: shared_user_title,
                    pending_terminal_start: shared_pending,
                    width_fraction: shared_width,
                    kind: shared_kind,
                    scope: shared_scope,
                } = pane;
                *shared_id == card.id
                    && shared_title == title
                    && shared_user_title.as_ref() == user_title
                    && *shared_pending == pending_terminal_start
                    && *shared_width == card.width_fraction
                    && shared_scope.as_deref() == scope
                    && views::is_shared_kind(&card.kind, shared_kind)
            }
        }
    }
}

/// Whether a new chat's first message may start its provider.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum FirstSendAdmission {
    /// The registry lists the pane, or will before the start reaches it.
    Admitted,
    /// The pane's scope is still loading or held back; the message waits.
    Waiting,
    /// The registry refused the scope's view, so its panes are not shared.
    Refused,
}

/// Which scopes need local presentation projected. A selected pass still
/// consults the complete canonical ownership set; it never treats a partial
/// pane map as proof that a resource is unowned.
#[derive(Clone, Copy)]
enum ViewProjection<'a> {
    All,
    /// A repository load follows the complete affected ownership closure while
    /// retaining structural admission, alias, notification and save semantics.
    Structural(&'a HashSet<Scope>),
    /// Proven presentation-only changes may defer local replica capture.
    Selected(&'a [Scope]),
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum ProjectionResource {
    Terminal(TerminalId),
    Chat(TerminalId),
    Browser(crate::model::BrowserId),
}

fn projection_resource(kind: &CardKind) -> Option<ProjectionResource> {
    match kind {
        CardKind::Terminal(session) => Some(ProjectionResource::Terminal(session.id)),
        CardKind::Chat(session) => Some(ProjectionResource::Chat(session.id)),
        CardKind::Browser(session) => Some(ProjectionResource::Browser(session.id)),
        _ => None,
    }
}

/// Each open card's worktree during one projection, with how many cards
/// each path holds, so a view no card resolves to is known without a scan.
#[derive(Default)]
struct CardWorktrees {
    paths: HashMap<CardId, PathBuf>,
    counts: HashMap<PathBuf, usize>,
}

impl CardWorktrees {
    #[cfg(test)]
    fn get(&self, card: CardId) -> Option<&PathBuf> {
        self.paths.get(&card)
    }

    fn holds(&self, path: &Path) -> bool {
        self.counts.contains_key(path)
    }

    /// The cards resolving to `path`, in no particular order.
    fn cards_in<'a>(&'a self, path: &'a Path) -> impl Iterator<Item = CardId> + 'a {
        self.paths
            .iter()
            .filter(move |(_, card_path)| card_path.as_path() == path)
            .map(|(&card, _)| card)
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

/// Root resolution and repository authority stay fixed during one synchronous
/// reconciliation; card membership and descriptors are projected afresh.
struct ViewScopeTopology<'a> {
    roots: worktree_ui::WorktreeRoots<'a>,
    authorities: ViewScopeAuthorities<'a>,
}

enum ViewScopeAuthorities<'a> {
    Borrowed {
        repositories: HashMap<&'a Path, &'a Path>,
        projects: HashSet<&'a Path>,
    },
    /// The topology discovery keeps between passes.
    Kept {
        repositories: &'a HashMap<PathBuf, PathBuf>,
        projects: &'a HashSet<PathBuf>,
    },
}

/// Whether `card` is a pane of the worktree view it resolves to: an open
/// card that is not a utility page.
fn shows_in_worktree(card: &CardState) -> bool {
    card.existence.is_open() && !card.kind.is_utility()
}

impl ViewScopeAuthorities<'_> {
    fn repository_for(&self, path: &Path) -> Option<&Path> {
        match self {
            Self::Borrowed { repositories, .. } => repositories.get(path).copied(),
            Self::Kept { repositories, .. } => repositories.get(path).map(PathBuf::as_path),
        }
    }

    fn contains_repository(&self, repository: &Path) -> bool {
        match self {
            Self::Borrowed { projects, .. } => projects.contains(repository),
            Self::Kept { projects, .. } => projects.contains(repository),
        }
    }
}

struct ScopeDiscovery<'a> {
    scopes: Vec<Cow<'a, Scope>>,
    worktree_cards: Vec<(&'a CardState, &'a Path)>,
    /// Every card a channel lists, by its identity, which is unique in a
    /// workspace.
    channeled: HashMap<CardId, &'a CardState>,
}

/// Preserve durable-card order within each logical resource. Canonical and
/// pending ownership use separate indexes because their alias sets differ.
fn unowned_aliases<'a>(
    cards: impl Iterator<Item = &'a CardState>,
    owned: &HashSet<CardId>,
) -> HashMap<ProjectionResource, Vec<&'a CardState>> {
    let mut aliases = HashMap::<_, Vec<_>>::new();
    for card in cards.filter(|card| card.existence.is_open() && !owned.contains(&card.id)) {
        if let Some(resource) = projection_resource(&card.kind) {
            aliases.entry(resource).or_default().push(card);
        }
    }
    aliases
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

/// A Close that could not take effect, said once the pane is shown again.
pub(in crate::app) const PANE_KEPT: &str =
    "This pane couldn't be removed right now, so it was kept. Close it again.";

/// A Close the registry kept the pane through, where this window has nothing
/// to show it in now, such as a worktree with no other open pane.
pub(super) const PANE_KEPT_ELSEWHERE: &str =
    "This pane couldn't be removed everywhere, so it may come back. Close it again if it does.";

/// A Close the registry refused or never received. The pane stays closed
/// here, under its identity, but other windows may still show it.
pub(super) const PANE_CLOSED_HERE_KEPT_ELSEWHERE: &str =
    "This pane closed here, but other windows may still show it. Close it there if they do.";

/// Any other pane edit the registry would not save: a rename, a resize, a
/// move or an added pane. This window then shows the panes as saved.
pub(super) const PANE_CHANGE_NOT_SAVED: &str = "A pane change couldn't be saved. Try it again.";

/// A pane's conversation or terminal changed elsewhere, as when another
/// window or its agent updated it, while this window was saving its own
/// change to it. The pane now shows that version.
pub(super) const PANE_CHANGED_ELSEWHERE: &str =
    "This pane changed elsewhere at the same time, so it now shows that version.";

/// [`PANE_CHANGED_ELSEWHERE`] once this window's rename or resize of the
/// pane has been saved again on top of that version.
pub(super) const PANE_CHANGED_ELSEWHERE_KEPT_LAYOUT: &str = "This pane changed elsewhere at the same time, so it now shows that version, with this window's name and width.";

/// What a fenced pane tells its reader, whatever its kind.
pub(super) const FENCED_PANE_EXPLANATION: &str = "cagents can't confirm that this pane's saved session is the one its agent is running now, so the pane stays as saved and won't reconnect. Other panes here work normally. Closing the pane doesn't stop its agent.";

/// Canonical managed panes whose saved descriptor no longer matches the
/// registry's actor, or whose card is ambiguous. Each one stays inert alone:
/// it neither projects nor publishes, while its scope's other panes proceed.
pub(super) fn quarantined_managed_panes(
    model: &crate::workspace_model::WorkspaceModel,
    views: &[View],
    display_index: &views::ManagedDisplayIndex<'_>,
) -> HashSet<CardId> {
    let managed = model
        .agents
        .iter()
        .map(|agent| (CardId::from_uuid(agent.id.as_uuid()), agent))
        .collect::<HashMap<_, _>>();
    views
        .iter()
        .flat_map(|view| &view.panes)
        .filter(|pane| {
            managed
                .get(&pane.id)
                .is_some_and(|agent| !display_index.permits_projection(agent, pane))
        })
        .map(|pane| pane.id)
        .collect()
}

/// Why one of `scopes` is not admitted by the registry yet.
fn view_scope_hold<'a>(
    state: &State,
    mut scopes: impl Iterator<Item = &'a Scope>,
) -> Option<&'static str> {
    scopes.find_map(|scope| {
        if !state.initialized.contains(scope) {
            Some("the shared workspace has not admitted its view")
        } else if state.seed_failures.contains_key(scope) {
            Some("the shared workspace refused its saved view")
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
    pub(super) removals: HashMap<CardId, Option<removal::RemovalSource>>,
    // Full snapshots advance view/agent authority. Metadata-only folds advance
    // the outer model sequence without supplying either of these collections.
    pub(super) snapshot_sequence: u64,
    /// [`Cagents::worktree_roots_digest`] as the last complete
    /// reconciliation read it.
    pub(super) reconciled_roots: u64,
    pub(super) terminal_source_workers: HashMap<
        CardId,
        (
            TerminalId,
            crate::workspace_model::views::TerminalRemovalBasis,
        ),
    >,
    pub(super) scope_absences: HashMap<Scope, u64>,
    pub(super) view_sequences: HashMap<Scope, u64>,
    pub(super) metadata_revisions: HashMap<super::workspace_model_sync::MetadataKey, u64>,
    /// Exact closed pane identities, retained across reconnects to this registry.
    pub(super) closed: HashSet<CardId>,
    #[cfg(test)]
    local_projection_count: std::cell::Cell<usize>,
    #[cfg(test)]
    pub(super) topology_build_count: std::cell::Cell<usize>,
    /// Structural passes, complete or over a repository's scopes.
    #[cfg(test)]
    pub(super) structural_pass_count: std::cell::Cell<usize>,
    #[cfg(test)]
    view_projection_count: std::cell::Cell<usize>,
    #[cfg(test)]
    selected_projection_count: std::cell::Cell<usize>,
    #[cfg(test)]
    scope_discovery_count: std::cell::Cell<usize>,
    #[cfg(test)]
    pub(super) notification_attribution_count: std::cell::Cell<usize>,
    #[cfg(test)]
    pane_attachment_count: usize,
    #[cfg(test)]
    pub(super) structural_tail_count: usize,
    #[cfg(test)]
    pub(super) chrome_tail_count: usize,
    #[cfg(test)]
    pub(super) publisher_index_entries: usize,
    #[cfg(test)]
    publisher_scope_comparisons: usize,
    /// Scopes complete publications left settled, publishing nothing.
    #[cfg(test)]
    pub(super) settled_publications: usize,
    /// Settled scopes a transfer reached, published from their baseline.
    #[cfg(test)]
    pub(super) transfer_unsettled: usize,
    #[cfg(test)]
    pub(super) chrome_cards_visited: std::cell::Cell<usize>,
    #[cfg(test)]
    order_rewrites: usize,
    #[cfg(test)]
    held_descriptor_clones: std::cell::Cell<usize>,
    /// The daemon answered this window without a shared model to give.
    pub(super) registry_unavailable: bool,
    /// This window joined the registry and has not yet told it which projects
    /// it has open; the next complete reconciliation does.
    pub(super) unintroduced_projects: bool,
    baseline: settlement::ScopeMap<Vec<Pane>>,
    initialized: settlement::ScopeSet,
    seeding: HashSet<Scope>,
    pub(super) seed_attempts: HashMap<Scope, uuid::Uuid>,
    /// Definitive failures stay held until an explicit retry or a new registry.
    pub(super) seed_failures: HashMap<Scope, String>,
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
    observed_terminals: HashMap<CardId, TerminalSession>,
    applying: bool,
    /// Panes `detach_shared_pane` detached that are still in the strip. The
    /// active strip's projection takes them out in its one edit, and
    /// `finish_shared_viewport_projection` takes out any left.
    strip_detached: HashSet<CardId>,
    pub creating_default: bool,
    /// An automatic empty pane waiting for this scope's shared panes, bound
    /// to the scope and directory that asked so navigating elsewhere cannot
    /// replay it into another channel.
    pub deferred_default: Option<(Scope, PathBuf)>,
    pub defaults: HashMap<CardId, TerminalSession>,
    default_requests: HashSet<CardId>,
    /// Scopes whose local aliases cannot yet adopt their canonical panes.
    pub blocked: HashSet<Scope>,
    /// Cards fenced from shared projection and publication, keeping their
    /// local presentation and saved evidence: managed panes that fail their
    /// registry proof, and the local aliases of those panes.
    pub quarantined: HashSet<CardId>,
    /// Fenced panes the reader closed, until no shared view holds them. Their
    /// close publishes a removal by identity, never a stale source.
    pub fenced_closes: HashSet<CardId>,
    pub adoptions: Vec<crate::persistence::TerminalPaneAdoption>,
    /// Chat cards whose shared view re-pointed them since this window last
    /// saved, which its own store must be told explicitly.
    pub chat_adoptions: HashMap<CardId, crate::session::ChatIdentityAdoption>,
    pub restorations: Vec<crate::persistence::TerminalPaneRestoration>,
    restoration_requests: HashMap<CardId, TerminalId>,
    /// What complete saves keep of scope discovery between passes.
    discovery: discovery::ViewDiscovery,
    /// Where each scope's view is in the model, kept between publications.
    view_index: view_index::ViewIndex,
}

impl State {
    /// Whether an empty local strip means `scope` is empty rather than not
    /// yet loaded: its shared panes have been admitted, or the registry
    /// refused its seed and this window keeps the scope's panes locally.
    pub(super) fn scope_settled(&self, scope: &Scope) -> bool {
        self.seed_failures.contains_key(scope)
            || (self.initialized.contains(scope)
                && !self.seeding.contains(scope)
                && !self.blocked.contains(scope))
    }

    /// What a reconciliation decides for this window's views, as text that
    /// reads the same whatever order its sets iterate in.
    #[cfg(test)]
    pub(super) fn reconciled_view(&self) -> String {
        use super::projection_oracle::sorted_entries;
        let set = |items: &mut dyn Iterator<Item = String>| {
            let mut items = items.collect::<Vec<_>>();
            items.sort_unstable();
            items.join(", ")
        };
        format!(
            "pending: {:?}\nblocked: {}\nquarantined: {}\nclosed: {}\nattached: {}\n\
             retained: {}\nseeding: {}\nseed attempts: {}\nseed failures: {}\n\
             requested transfers: {}\ndefaults: {}\nstrip detached: {}\n\
             closed chats: {}\nobserved chats: {}\nobserved terminals: {}\n\
             adoptions: {:?}\nchat adoptions: {}\nrestorations: {:?}\n\
             restoration requests: {}\ndeferred default: {:?}\nfenced closes: {}",
            self.pending,
            set(&mut self.blocked.iter().map(|scope| format!("{scope:?}"))),
            set(&mut self.quarantined.iter().map(|id| format!("{id:?}"))),
            set(&mut self.closed.iter().map(|id| format!("{id:?}"))),
            set(&mut self.attached.iter().map(|id| format!("{id:?}"))),
            set(&mut self.retained.iter().map(|id| format!("{id:?}"))),
            set(&mut self.seeding.iter().map(|scope| format!("{scope:?}"))),
            set(&mut self.seed_attempts.keys().map(|scope| format!("{scope:?}"))),
            sorted_entries(&self.seed_failures),
            sorted_entries(&self.requested_transfers),
            set(&mut self.defaults.keys().map(|id| format!("{id:?}"))),
            set(&mut self.strip_detached.iter().map(|id| format!("{id:?}"))),
            sorted_entries(&self.closed_chats),
            sorted_entries(&self.observed_chats),
            sorted_entries(&self.observed_terminals),
            self.adoptions,
            sorted_entries(&self.chat_adoptions),
            self.restorations,
            sorted_entries(&self.restoration_requests),
            self.deferred_default,
            set(&mut self.fenced_closes.iter().map(|id| format!("{id:?}"))),
        )
    }

    #[cfg(test)]
    pub(super) fn seeding_scopes(&self) -> Vec<Scope> {
        self.seeding.iter().cloned().collect()
    }

    /// Stops comparing each complete pass with a rediscovery, for timing
    /// workloads that measure what a pass costs.
    #[cfg(test)]
    pub(super) fn skip_rediscovery_check(&mut self) {
        self.discovery.unchecked = true;
    }

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
            .any(|(_, _, change)| matches!(change, Change::Update { pane, .. } | Change::UpdateTerminalSource { pane, .. } if pane.id == card))
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
                if card.existence.is_open()
                    && let CardKind::Terminal(session) = &card.kind
                {
                    return Some((card.id, session.id));
                }
                None
            })
            .collect::<HashMap<_, _>>();
        self.restoration_requests
            .retain(|card, terminal| open.get(card) == Some(terminal));
    }

    /// A copy of everything a publication reads or writes, so a test can
    /// publish the same state two ways. The copy keeps no discovery or view
    /// index, and its key sets are new sets.
    #[cfg(test)]
    pub(super) fn publication_copy(&self) -> Self {
        let Self {
            removals,
            snapshot_sequence,
            reconciled_roots: _,
            terminal_source_workers,
            scope_absences,
            view_sequences,
            metadata_revisions,
            closed,
            local_projection_count: _,
            topology_build_count: _,
            structural_pass_count: _,
            view_projection_count: _,
            selected_projection_count: _,
            scope_discovery_count: _,
            notification_attribution_count: _,
            pane_attachment_count: _,
            structural_tail_count: _,
            chrome_tail_count: _,
            publisher_index_entries: _,
            publisher_scope_comparisons: _,
            settled_publications: _,
            transfer_unsettled: _,
            chrome_cards_visited: _,
            order_rewrites: _,
            held_descriptor_clones: _,
            registry_unavailable,
            unintroduced_projects,
            baseline,
            initialized,
            seeding,
            seed_attempts,
            seed_failures,
            pending,
            source_bases,
            requested_transfers,
            attached,
            retained,
            closed_chats,
            observed_chats,
            observed_terminals,
            applying,
            strip_detached,
            creating_default,
            deferred_default,
            defaults,
            default_requests,
            blocked,
            quarantined,
            fenced_closes,
            adoptions,
            chat_adoptions,
            restorations,
            restoration_requests,
            discovery: _,
            view_index: _,
        } = self;
        Self {
            removals: removals.clone(),
            snapshot_sequence: *snapshot_sequence,
            terminal_source_workers: terminal_source_workers.clone(),
            scope_absences: scope_absences.clone(),
            view_sequences: view_sequences.clone(),
            metadata_revisions: metadata_revisions.clone(),
            closed: closed.clone(),
            registry_unavailable: *registry_unavailable,
            unintroduced_projects: *unintroduced_projects,
            baseline: baseline.clone(),
            initialized: initialized.clone(),
            seeding: seeding.clone(),
            seed_attempts: seed_attempts.clone(),
            seed_failures: seed_failures.clone(),
            pending: pending.clone(),
            source_bases: source_bases.clone(),
            requested_transfers: requested_transfers.clone(),
            attached: attached.clone(),
            retained: retained.clone(),
            closed_chats: closed_chats.clone(),
            observed_chats: observed_chats.clone(),
            observed_terminals: observed_terminals.clone(),
            applying: *applying,
            strip_detached: strip_detached.clone(),
            creating_default: *creating_default,
            deferred_default: deferred_default.clone(),
            defaults: defaults.clone(),
            default_requests: default_requests.clone(),
            blocked: blocked.clone(),
            quarantined: quarantined.clone(),
            fenced_closes: fenced_closes.clone(),
            adoptions: adoptions.clone(),
            chat_adoptions: chat_adoptions.clone(),
            restorations: restorations.clone(),
            restoration_requests: restoration_requests.clone(),
            ..Self::default()
        }
    }

    pub(super) fn for_workspace(workspace: &WorkspaceState) -> Self {
        Self {
            retained: workspace.local_only_cards.clone(),
            attached: workspace
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
                .collect(),
            ..Self::default()
        }
    }
}

impl Cagents {
    pub(super) fn capture_view_update_source(&mut self, edit: &ModelEdit) {
        let ModelEdit::ChangeView {
            scope,
            operation,
            change: Change::Update { .. } | Change::UpdateTerminalSource { .. },
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
        self.install_workspace_model_snapshot(model);
        self.refuse_conflicting_view_change(scope, operation, change, error)
    }

    pub(super) fn refuse_conflicting_view_change(
        &mut self,
        scope: Scope,
        operation: uuid::Uuid,
        change: Change,
        error: String,
    ) -> Task<Message> {
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
        if let Change::RemoveSource { before, .. } = &change {
            self.shared_views.pending.remove(index);
            self.shared_views.source_bases.remove(&operation);
            let card = before.id;
            let projected = self.reconcile_workspace_model();
            self.report_refused_close(&scope, operation, card, &error);
            return projected;
        }
        // Another window can publish the same session update first, as when two
        // windows attached to one conversation both learn its provider session.
        // Only a session update qualifies: a refused name or width edit can be lost.
        let session_already_applied = matches!(&change, Change::Update { before, pane } | Change::UpdateTerminalSource { before, pane, .. } if {
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
        if let Change::Update { before, pane } | Change::UpdateTerminalSource { before, pane, .. } =
            &change
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
                && !self.shared_views.quarantined.contains(&pane.id)
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
                        matches!(change, Change::Update { before, pane: after } | Change::UpdateTerminalSource { before, pane: after, .. }
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
                || matches!(&change, Change::Update { before, pane } | Change::UpdateTerminalSource { before, pane, .. }
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
        let retried_layout = retry.is_some();
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
        // A pane this window has closed, or is closing, shows nothing more,
        // so the version the registry kept is nothing to tell anyone about.
        let shown = matches!(&change, Change::Update { pane, .. } | Change::UpdateTerminalSource { pane, .. }
            if self.card_state(pane.id).is_some_and(|card| card.existence.is_open())
                && self
                    .workspace
                    .pending_close()
                    .is_none_or(|close| close.unmounting != pane.id));
        if !already_applied && shown {
            self.notices
                .record(TransientNotice::warning(if retried_layout {
                    PANE_CHANGED_ELSEWHERE_KEPT_LAYOUT
                } else {
                    PANE_CHANGED_ELSEWHERE
                }));
        }
        self.reconcile_workspace_model()
    }

    /// Settle what the person is told about a Close the registry refused,
    /// once this window has projected what the registry holds. A pane the
    /// registry still holds is kept, and said to be. A pane it no longer
    /// holds is closed, as the person asked: its agent or another window may
    /// have closed it first, or its project or channel may be gone.
    pub(super) fn report_refused_close(
        &mut self,
        scope: &Scope,
        operation: uuid::Uuid,
        card: CardId,
        error: &str,
    ) {
        let kept = self
            .workspace_model
            .views
            .iter()
            .any(|view| view.panes.iter().any(|pane| pane.id == card));
        self.index_cards();
        if kept && self.closed_here(card) {
            // The pane stays closed here whatever the registry kept: storage
            // holds its tombstone, and the view does not bring it back.
            tracing::warn!(?scope, %operation, %error, %card, "the registry kept a pane closed here");
            self.notices
                .record(TransientNotice::warning(PANE_CLOSED_HERE_KEPT_ELSEWHERE));
            return;
        }
        if kept && self.closes_in_background(card) {
            // Its removal was judged against the conversation still running,
            // which is stopping; the pane, out of sight, goes once it has.
            tracing::debug!(?scope, %operation, %error, %card, "the registry kept a closed pane while its conversation stops");
        } else if kept {
            tracing::warn!(?scope, %operation, %error, %card, "the registry refused a close and kept its pane");
            let shown = self.workspace.order().any(|id| id == card);
            self.notices.record(TransientNotice::warning(if shown {
                PANE_KEPT
            } else {
                PANE_KEPT_ELSEWHERE
            }));
        } else {
            tracing::debug!(?scope, %operation, %error, %card, "a refused close's pane is already gone");
        }
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
        // A fenced pane's saved session is not the one its agent runs, so
        // nothing attaches or relaunches it, a retry included.
        self.shared_views.quarantined.contains(&card)
            || self.shared_views.defaults.contains_key(&card)
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

    /// Every card the registry shows in some view.
    fn registry_listed_panes(&self) -> HashSet<CardId> {
        self.workspace_model
            .views
            .iter()
            .flat_map(|view| view.panes.iter().map(|pane| pane.id))
            .collect()
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
        self.release_held_terminals_with_topology(None)
    }

    fn release_held_terminals_with_topology(
        &mut self,
        topology: Option<&ViewScopeTopology<'_>>,
    ) -> Task<Message> {
        if self.shared_registry_connecting() {
            return Task::none();
        }
        self.retire_startup_registry_waits();
        // Which panes the registry lists, read at most once rather than by a
        // walk of every view for each candidate. The release leaves the
        // model's views as they are.
        let listed = std::cell::OnceCell::new();
        let listed_in_registry = |card: CardId| {
            listed
                .get_or_init(|| self.registry_listed_panes())
                .contains(&card)
        };
        let held = self
            .durable
            .cards
            .iter()
            .filter_map(|card| match &card.kind {
                CardKind::Terminal(session)
                    if !terminal_ended(session)
                        && card.existence.is_open()
                        && matches!(self.cards.get(&card.id), Some(CardRuntime::Loading))
                        && self.shared_views.attached.contains(&card.id)
                        && !self.shared_views.defaults.contains_key(&card.id)
                        && !self.durable.pending_terminal_creations.contains(&card.id)
                        && !self.terminal_start_in_flight(card.id)
                        && !self.terminal_retry_pending(card.id)
                        && !self.pane_close_owned(card.id) =>
                {
                    let opening = matches!(self.residency(card.id), Residency::Hydrating { .. });
                    let unlisted = !listed_in_registry(card.id);
                    if !opening && !unlisted {
                        return None;
                    }
                    #[cfg(test)]
                    self.shared_views
                        .held_descriptor_clones
                        .set(self.shared_views.held_descriptor_clones.get() + 1);
                    Some((
                        card.id,
                        self.exposure(card.id),
                        opening,
                        unlisted,
                        session.clone(),
                    ))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        if held.is_empty() {
            return Task::none();
        }
        // Only an unlisted unopened candidate can release its custody after
        // global admission. Listed passive panes perform no release work, and
        // opening panes already bypass this barrier. Resolve the proof before
        // any release mutates attached custody.
        let needs_scope_admission = held
            .iter()
            .any(|&(_, _, opening, unlisted, _)| !opening && unlisted);
        let views_settled = !needs_scope_admission
            || self.shared_views.registry_unavailable
            || self
                .shared_views_startup_hold_with_topology(topology)
                .is_none();
        let confirmed = self.workspace_model.identity != crate::workspace_model::NO_REGISTRY;
        let mut tasks = Vec::new();
        let mut hydrate = false;
        for (card, exposure, opening, unlisted, session) in held {
            // An unopened terminal waits until the view that may yet list it
            // has been admitted, exactly as before candidate collection.
            if !views_settled && !opening {
                continue;
            }
            if unlisted {
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

    /// The accepted automatic start, including a save still awaiting Retry.
    pub(super) fn pending_terminal_reservation(&self, card: CardId) -> Option<&Pane> {
        self.workspace_model
            .views
            .iter()
            .flat_map(|view| &view.panes)
            .find(|pane| pane.id == card && pane.pending_terminal_start)
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
        self.shared_views_startup_hold_with_topology(None)
    }

    fn shared_views_startup_hold_with_topology(
        &self,
        topology: Option<&ViewScopeTopology<'_>>,
    ) -> Option<&'static str> {
        if self.durable.workspace_model_identity.is_none() && self.shared_views.attached.is_empty()
        {
            return None;
        }
        if self.workspace_model.identity == crate::workspace_model::NO_REGISTRY {
            return Some("the shared workspace has not answered this window");
        }
        let discovery = match topology {
            Some(topology) => self.discover_view_scopes_with(topology),
            None => self.discover_view_scopes(),
        };
        view_scope_hold(
            &self.shared_views,
            discovery.scopes.iter().map(|scope| &**scope),
        )
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
        if self.shared_views.quarantined.contains(&card) {
            return Some("its saved session is not the one its agent is running now");
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

    pub(super) fn local_view_scopes(&self) -> Vec<Scope> {
        self.discover_view_scopes()
            .scopes
            .into_iter()
            .map(Cow::into_owned)
            .collect()
    }

    /// Preserve the first matching project's authority, just as
    /// worktree_view_scope does, including its common Git directory's parent.
    fn view_repository_paths(&self) -> impl Iterator<Item = (&Path, &Path)> {
        self.sidebar_model.projects.iter().flat_map(|project| {
            let repository = project.repository.as_path();
            project
                .worktrees
                .iter()
                .map(|worktree| worktree.path.as_path())
                .chain(std::iter::once(repository))
                .chain(
                    repository
                        .file_name()
                        .is_some_and(|name| name == ".git")
                        .then(|| repository.parent())
                        .flatten(),
                )
                .map(move |path| (path, repository))
        })
    }

    /// `view_repository_paths`, owned, keeping each path's first authority.
    fn owned_view_repositories(&self) -> HashMap<PathBuf, PathBuf> {
        let mut repositories = HashMap::new();
        for (path, repository) in self.view_repository_paths() {
            repositories
                .entry(path.to_owned())
                .or_insert_with(|| repository.to_owned());
        }
        repositories
    }

    #[cfg(test)]
    fn record_topology_build(&self) {
        self.shared_views
            .topology_build_count
            .set(self.shared_views.topology_build_count.get() + 1);
    }

    /// Standalone immutable reads borrow path storage. In particular, repeated
    /// startup readiness checks must not clone every saved repository path.
    fn borrowed_view_scope_topology(&self) -> ViewScopeTopology<'_> {
        #[cfg(test)]
        self.record_topology_build();
        let mut repositories = HashMap::new();
        for (path, repository) in self.view_repository_paths() {
            repositories.entry(path).or_insert(repository);
        }
        ViewScopeTopology {
            roots: self.indexed_worktree_roots(),
            authorities: ViewScopeAuthorities::Borrowed {
                repositories,
                projects: self
                    .sidebar_model
                    .projects
                    .iter()
                    .map(|project| project.repository.as_path())
                    .collect(),
            },
        }
    }

    /// Every scope this window shows, and the worktree each card outside a
    /// channel resolves to. A large workspace holds thousands of cards and
    /// historical worktrees, so each card and path is resolved by lookup
    /// rather than by comparing it with every root or project worktree, and a
    /// scope the registry already lists is borrowed rather than copied.
    fn discover_view_scopes(&self) -> ScopeDiscovery<'_> {
        let topology = self.borrowed_view_scope_topology();
        self.discover_view_scopes_with(&topology)
    }

    /// As `discover_view_scopes`, over topology a reconciliation already
    /// built; membership is still read afresh.
    fn discover_view_scopes_with<'a>(
        &'a self,
        topology: &ViewScopeTopology<'a>,
    ) -> ScopeDiscovery<'a> {
        #[cfg(test)]
        self.shared_views
            .scope_discovery_count
            .set(self.shared_views.scope_discovery_count.get() + 1);
        let channel_cards = self
            .durable
            .channels
            .iter()
            .flat_map(|channel| channel.cards.iter().copied())
            .collect::<HashSet<_>>();
        let mut channeled = HashMap::with_capacity(channel_cards.len());
        let mut worktree_cards = Vec::new();
        for card in self.durable.cards.iter() {
            if channel_cards.contains(&card.id) {
                channeled.insert(card.id, card);
            } else if let Some(path) = self.unchanneled_card_worktree_indexed(card, &topology.roots)
            {
                worktree_cards.push((card, path));
            }
        }
        let mut channels = HashSet::new();
        let mut scopes = self
            .durable
            .channels
            .iter()
            .filter(|channel| channels.insert(channel.id))
            .map(|channel| Cow::Owned(Scope::Channel(channel.id)))
            .collect::<Vec<_>>();
        // Worktree scopes are told apart by their repository and path, which
        // every candidate below borrows from this window or its topology.
        let mut worktrees = HashSet::<(&Path, &Path)>::new();
        for view in &self.workspace_model.views {
            if let Scope::Worktree { repository, path } = &view.scope
                && topology.authorities.contains_repository(repository)
                && worktrees.insert((repository.as_path(), path.as_path()))
            {
                scopes.push(Cow::Borrowed(&view.scope));
            }
        }
        for path in self
            .known_worktree_roots
            .iter()
            .map(PathBuf::as_path)
            .chain(self.active_workspace.as_deref())
            .chain(worktree_cards.iter().map(|(_, path)| *path))
        {
            if let Some(repository) = topology.authorities.repository_for(path)
                && worktrees.insert((repository, path))
            {
                scopes.push(Cow::Owned(Scope::Worktree {
                    repository: repository.to_owned(),
                    path: path.to_owned(),
                }));
            }
        }
        ScopeDiscovery {
            scopes,
            worktree_cards,
            channeled,
        }
    }

    #[cfg(test)]
    /// The local views that list `card`, as `local_view_cards` would,
    /// without resolving every view's cards to find one.
    fn local_scopes_listing(&self, card: CardId) -> Vec<Scope> {
        if self
            .card_state(card)
            .is_none_or(|card| !card.existence.is_open())
        {
            return Vec::new();
        }
        let topology = self.borrowed_view_scope_topology();
        self.local_scopes_listing_with_topology(card, &topology)
    }

    fn local_scopes_listing_with_topology(
        &self,
        card: CardId,
        topology: &ViewScopeTopology<'_>,
    ) -> Vec<Scope> {
        if self
            .card_state(card)
            .is_none_or(|card| !card.existence.is_open())
        {
            return Vec::new();
        }
        let ScopeDiscovery {
            scopes,
            worktree_cards,
            ..
        } = self.discover_view_scopes_with(topology);
        let worktree = worktree_cards
            .iter()
            .find(|(state, _)| state.id == card)
            .filter(|(state, _)| shows_in_worktree(state))
            .map(|(_, path)| *path);
        scopes
            .into_iter()
            .filter(|scope| match &**scope {
                Scope::Channel(id) => self
                    .durable
                    .channels
                    .iter()
                    .any(|channel| channel.id == *id && channel.cards.contains(&card)),
                Scope::Worktree { path, .. } => worktree == Some(path.as_path()),
            })
            .map(Cow::into_owned)
            .collect()
    }

    fn local_view_cards(&self, scope: &Scope) -> Vec<CardId> {
        match scope {
            Scope::Channel(id) => {
                let mounted = if self.active_workspace.is_some()
                    && self.durable.active_channel == Some(*id)
                {
                    self.active_channel_order().map(|(_, order)| order)
                } else {
                    None
                };
                let members = mounted
                    .or_else(|| {
                        self.durable
                            .channels
                            .iter()
                            .find(|channel| channel.id == *id)
                            .map(|channel| channel.cards.clone())
                    })
                    .unwrap_or_default();
                members
                    .into_iter()
                    .filter(|id| {
                        self.card_state(*id)
                            .is_some_and(|card| card.existence.is_open())
                    })
                    .collect()
            }
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
                        shows_in_worktree(card)
                            && !channel_cards.contains(&card.id)
                            && self.unchanneled_card_worktree_indexed(card, &roots)
                                == Some(path.as_path())
                    })
                    .map(|card| card.id)
                    .collect()
            }
        }
    }

    #[cfg(test)]
    fn hold_reported_title(&self, scope: &Scope, pane: &mut Pane) {
        if let Some(title) = self.held_reported_title(scope, pane.id) {
            pane.title = title.to_owned();
        }
    }

    /// A reported title waiting for its deferred save is not published yet:
    /// the pane keeps the title this window last shared for it, so neither an
    /// edit nor a captured baseline carries the newer one before that save.
    fn held_reported_title(&self, scope: &Scope, card: CardId) -> Option<&str> {
        if !self.reported_title_save.cards.contains(&card) {
            return None;
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
        in_scope
            .chain(elsewhere)
            .flatten()
            .find(|shared| shared.id == card)
            .map(|shared| shared.title.as_str())
    }

    /// `card` as a pane of `scope`, or `None` if the scope publishes no pane
    /// for it. `listed_pending` says whether a view lists its pane with a
    /// terminal start pending.
    fn local_pane<'a>(
        &'a self,
        scope: &'a Scope,
        card: &'a CardState,
        listed_pending: impl FnOnce(CardId) -> bool,
    ) -> Option<LocalPane<'a>> {
        let id = card.id;
        if self.shared_views.retained.contains(&id)
            || self.chat_is_unused_default(id)
            || self.shared_views.defaults.contains_key(&id)
            // Published once it starts, under the identity it starts with.
            || self.reopen_pending(id)
            // What `Pane::from_card` publishes.
            || !card.existence.is_open()
            || card.kind.is_utility()
        {
            return None;
        }
        if let Some(held) = self.unsaved_terminal_pane(scope, id) {
            return Some(LocalPane::Held(held));
        }
        Some(LocalPane::Card {
            card,
            title: self
                .held_reported_title(scope, id)
                .unwrap_or(card.title.as_str()),
            user_title: self.chrome_ui.user_titles.get(&id),
            pending_terminal_start: matches!(card.kind, CardKind::Terminal(_))
                && (self.durable.pending_terminal_creations.contains(&id) || listed_pending(id)),
            scope: match scope {
                Scope::Worktree { path, .. } => Some(path.as_path()),
                Scope::Channel(_) => self.durable.card_scopes.get(&id).map(PathBuf::as_path),
            },
        })
    }

    fn local_view_panes(&self, scope: &Scope) -> Vec<Pane> {
        self.local_view_cards(scope)
            .into_iter()
            .filter_map(|id| {
                self.local_pane(scope, self.card_state(id)?, |id| {
                    self.workspace_model
                        .views
                        .iter()
                        .flat_map(|view| &view.panes)
                        .any(|shared| shared.id == id && shared.pending_terminal_start)
                })
            })
            .map(|pane| pane.to_pane())
            .collect()
    }

    #[cfg(test)]
    pub(super) fn initialize_terminal_save_test_scope(&mut self, scope: Scope) {
        self.shared_views.initialized.insert(scope);
    }

    #[cfg(test)]
    pub(super) fn local_views_for_terminal_save_test(&self, scope: &Scope) -> Pane {
        let direct = self.local_view_panes(scope);
        let batched = self.local_views_in(Some(&HashSet::from([scope.clone()])));
        assert_eq!(direct, batched.panes[scope]);
        direct[0].clone()
    }

    fn local_views(&self) -> LocalViews {
        self.local_views_in(None)
    }

    /// The local side of every discovered scope, or of those `selected`, that
    /// a publication can act on.
    ///
    /// Most scopes a long-lived window discovers are historical worktrees:
    /// already seeded, published empty and holding no card here. Publishing
    /// one diffs nothing against nothing, so it is left out, and a pass costs
    /// what the window shows rather than everything it ever showed. A scope
    /// is kept while it is not yet seeded, has no settled publication, still
    /// lists panes this window published, or holds a card here; in any of
    /// those a publication may seed it, change it or settle its baseline.
    fn local_views_in(&self, selected: Option<&HashSet<Scope>>) -> LocalViews {
        let topology = self.borrowed_view_scope_topology();
        self.local_views_with_topology(selected, &topology)
    }

    fn local_views_with_topology<'a>(
        &'a self,
        selected: Option<&HashSet<Scope>>,
        topology: &ViewScopeTopology<'a>,
    ) -> LocalViews {
        let mut phase = WorkspaceProjectionTiming::new(selected.is_none());
        let discovery = self.discover_view_scopes_with(topology);
        phase.mark("local_discover");
        self.project_local_views(selected, discovery, false, phase)
    }

    /// The local side of the `publishable` scopes among those discovered.
    /// With `settle`, a seeded scope whose panes equal its baseline is left
    /// out of `panes` and listed as settled instead.
    fn project_local_views<'a>(
        &'a self,
        selected: Option<&HashSet<Scope>>,
        discovery: ScopeDiscovery<'a>,
        settle: bool,
        mut phase: WorkspaceProjectionTiming,
    ) -> LocalViews {
        #[cfg(test)]
        self.shared_views
            .local_projection_count
            .set(self.shared_views.local_projection_count.get() + 1);
        let ScopeDiscovery {
            scopes,
            worktree_cards,
            channeled,
        } = discovery;
        let mut by_path = HashMap::<&Path, Vec<&CardState>>::new();
        for (card, path) in worktree_cards {
            if shows_in_worktree(card) {
                by_path.entry(path).or_default().push(card);
            }
        }
        let channel_cards = self
            .durable
            .channels
            .iter()
            .map(|channel| {
                // A closed card stays saved in its channel but is no pane.
                let members = channel
                    .cards
                    .iter()
                    .filter_map(|id| channeled.get(id).copied())
                    .filter(|card| card.existence.is_open())
                    .collect::<Vec<_>>();
                (channel.id, members)
            })
            .collect::<HashMap<_, _>>();
        let local_cards = |scope: &Scope| match scope {
            Scope::Channel(id) => channel_cards.get(id).map(Vec::as_slice),
            Scope::Worktree { path, .. } => by_path.get(path.as_path()).map(Vec::as_slice),
        };
        // Why each kept scope is kept, for the trace: unseeded, unsettled,
        // published panes, local cards.
        let mut kept = [0usize; 4];
        let mut publishable = |scope: &Scope| {
            let reason = if !self.shared_views.initialized.contains(scope) {
                0
            } else {
                match self.shared_views.baseline.get(scope) {
                    None => 1,
                    Some(published) if !published.is_empty() => 2,
                    Some(_) if local_cards(scope).is_some_and(|cards| !cards.is_empty()) => 3,
                    Some(_) => return false,
                }
            };
            kept[reason] += 1;
            true
        };
        let discovered = scopes.len();
        let scopes = scopes
            .into_iter()
            .filter(|scope| selected.is_none_or(|selected| selected.contains(&**scope)))
            .filter(|scope| publishable(scope))
            .map(Cow::into_owned)
            .collect::<Vec<_>>();
        tracing::debug!(target: "cagents::workspace_pipeline", stage = "local_scopes",
            discovered, projected = scopes.len(), unseeded = kept[0], unsettled = kept[1],
            published = kept[2], local = kept[3], "Selected the scopes to project");
        let pending = self
            .workspace_model
            .views
            .iter()
            .flat_map(|view| &view.panes)
            .filter(|pane| pane.pending_terminal_start)
            .map(|pane| pane.id)
            .collect::<HashSet<_>>();
        phase.mark("local_members");
        let mut members = HashMap::with_capacity(scopes.len());
        let mut panes = HashMap::with_capacity(scopes.len());
        let mut settled = HashSet::new();
        for scope in &scopes {
            let listed = local_cards(scope).unwrap_or_default();
            let projected = || {
                listed
                    .iter()
                    .filter_map(|card| self.local_pane(scope, card, |id| pending.contains(&id)))
            };
            members.insert(scope.clone(), listed.iter().map(|card| card.id).collect());
            // An unseeded scope publishes its seed whatever its baseline. A
            // baseline listing a chat yet to be published is no settlement:
            // a shared projection takes that chat out of it to publish again.
            let settles = settle
                && self.shared_views.initialized.contains(scope)
                && self
                    .shared_views
                    .baseline
                    .get(scope)
                    .is_some_and(|baseline| {
                        LocalPane::all_match(projected(), baseline)
                            && !baseline.iter().any(|pane| self.unpublished_chat(pane.id))
                    });
            if settles {
                settled.insert(scope.clone());
            } else {
                panes.insert(
                    scope.clone(),
                    projected().map(|pane| pane.to_pane()).collect(),
                );
            }
        }
        tracing::debug!(target: "cagents::workspace_pipeline", stage = "local_settled",
            settled = settled.len(), projected = panes.len(),
            "Compared the projected scopes with their baselines");
        phase.mark("local_panes");
        LocalViews {
            scopes,
            cards: members,
            panes,
            settled,
        }
    }

    /// A selected projection is safe only while this window already holds the
    /// same membership and sources. Presentation may differ because local
    /// pending intent or a held reported title is layered over the shared row.
    pub(super) fn presentation_projection_ready(
        &self,
        before: &View,
        after: &View,
        pending_operation: Option<uuid::Uuid>,
    ) -> Result<(), &'static str> {
        let Scope::Channel(channel) = &before.scope else {
            return Err("derived worktree membership");
        };
        if before.scope != after.scope
            || after.revision < before.revision
            || (after.revision == before.revision && after != before)
            || before.retired != after.retired
            || before.panes.len() != after.panes.len()
            || !before.panes.iter().zip(&after.panes).all(|(a, b)| {
                a.id == b.id
                    && a.kind == b.kind
                    && a.scope == b.scope
                    && a.pending_terminal_start == b.pending_terminal_start
            })
        {
            return Err("scope structure or source changed");
        }
        if !self.shared_views.initialized.contains(&before.scope)
            || self.shared_views.blocked.contains(&before.scope)
            // A fenced pane keeps its local presentation, which only the
            // complete projection places.
            || before
                .panes
                .iter()
                .any(|pane| self.shared_views.quarantined.contains(&pane.id))
            || self.shared_views.seeding.contains(&before.scope)
            || self.shared_views.seed_failures.contains_key(&before.scope)
            || self.channel_ui.is_deleting(*channel)
            || self.channel_ui.was_deleted(*channel)
        {
            return Err("scope admission is unsettled");
        }
        let selected: HashSet<_> = before.panes.iter().map(|pane| pane.id).collect();
        if self.navigation_save.is_some()
            || self.shared_views.creating_default
            || self.shared_views.deferred_default.is_some()
            || !self.shared_views.requested_transfers.is_empty()
            || self.shared_views.adoptions.iter().any(|adoption| {
                selected.contains(&adoption.alias) || selected.contains(&adoption.canonical)
            })
            || self
                .shared_views
                .restorations
                .iter()
                .any(|restoration| selected.contains(&restoration.canonical))
            || self
                .shared_views
                .chat_adoptions
                .keys()
                .any(|id| selected.contains(id))
            || self
                .durable
                .pending_terminal_creations
                .iter()
                .any(|id| selected.contains(id))
            || self
                .restored_agent_attaches
                .iter()
                .any(|id| selected.contains(id))
            || !self.channel_ui.retire_candidates.is_empty()
            || !self.channel_ui.retiring.is_empty()
            || !self.project_ui.closing.is_empty()
            || !self.project_ui.removing.is_empty()
            || !self.project_ui.reopening.is_empty()
            || !self.queued_card_closes.is_empty()
            || self.workspace.pending_close().is_some()
        {
            return Err("workspace lifecycle is unsettled");
        }
        // These local chats still need membership published. An unrelated
        // draft explicitly retained by this window is not such an obligation.
        if self.holds_unpublished_local_chat() {
            return Err("an unpublished local conversation needs reconciliation");
        }
        match pending_operation {
            Some(operation)
                if self.shared_views.pending.len() == 1
                    && self.shared_views.pending[0].0 == operation
                    && self.shared_views.pending[0].1 == before.scope => {}
            None if self.shared_views.pending.is_empty() => {}
            _ => return Err("unrelated pending intent"),
        }
        let Some(baseline) = self.shared_views.baseline.get(&before.scope) else {
            return Err("no settled publication baseline");
        };
        if baseline.len() != before.panes.len()
            || !baseline
                .iter()
                .zip(&before.panes)
                .all(|(a, b)| a.id == b.id && a.kind == b.kind && a.scope == b.scope)
        {
            return Err("publication membership is unsettled");
        }
        let Some(local) = self.durable.channels.iter().find(|row| row.id == *channel) else {
            return Err("channel is not projected");
        };
        let Some(shared) = self.workspace_model.channel(*channel) else {
            return Err("channel metadata is not held");
        };
        if !local.shared
            || local.title != shared.title
            || local.title_source != shared.title_source
            || shared.ordinal == crate::workspace_model::PENDING_ORDINAL
            || local.ordinal != shared.ordinal
            || self.project_for_repository(&shared.repository) != Some(local.project_id)
            || !self
                .workspace_model
                .projects
                .iter()
                .any(|project| project.repository == shared.repository)
        {
            return Err("channel metadata is unsettled");
        }
        // Canonical ownership alone cannot prove that an older local
        // placement has already been cleared. The complete projector repairs
        // that inconsistency before persisting; a selected pass must defer.
        if self
            .durable
            .channels
            .iter()
            .any(|other| other.id != *channel && other.cards.iter().any(|id| selected.contains(id)))
        {
            return Err("local pane placement is ambiguous");
        }
        let current: Vec<_> = local
            .cards
            .iter()
            .copied()
            .filter(|id| self.card_state(*id).is_some_and(shows_in_worktree))
            .collect();
        if current != before.panes.iter().map(|pane| pane.id).collect::<Vec<_>>() {
            return Err("local membership is unsettled");
        }
        for pane in &before.panes {
            let Some(card) = self.card_state(pane.id) else {
                return Err("pane is not projected");
            };
            if !card.existence.is_open()
                || views::shared_kind(&card.kind) != pane.kind
                || self.durable.card_scopes.get(&pane.id) != pane.scope.as_ref()
                || pane.pending_terminal_start
                || self.shared_views.retained.contains(&pane.id)
                || self.shared_views.defaults.contains_key(&pane.id)
                || self.pane_move_lifecycle_pending(pane.id)
                || self.chat_awaiting_admission(pane.id)
                || self.saved_terminals.entries.contains_key(&pane.id)
                || !self.cards.contains_key(&pane.id)
                || matches!(self.cards.get(&pane.id), Some(CardRuntime::Loading))
                // A retained browser failure is a settled presentation: the
                // existing reducer preserves it while this exact source and
                // runtime remain present. Other failed surfaces may still owe
                // admission or recovery work.
                || (matches!(self.cards.get(&pane.id), Some(CardRuntime::Failed(_)))
                    && !matches!(pane.kind, CardKind::Browser(_)))
            {
                return Err("pane source or runtime is unsettled");
            }
        }
        // This census deliberately covers every canonical scope. A selected
        // channel cannot establish resource ownership or authorize alias
        // retirement from its own pane list alone.
        let mut owned = HashSet::new();
        let mut resources = HashSet::new();
        for pane in self
            .workspace_model
            .views
            .iter()
            .flat_map(|view| &view.panes)
        {
            if !owned.insert(pane.id)
                || self.shared_views.closed.contains(&pane.id)
                || projection_resource(&pane.kind)
                    .is_some_and(|resource| !resources.insert(resource))
            {
                return Err("canonical ownership is ambiguous");
            }
        }
        if self
            .durable
            .cards
            .iter()
            .filter(|card| card.existence.is_open() && !owned.contains(&card.id))
            .any(|card| {
                projection_resource(&card.kind)
                    .is_some_and(|resource| resources.contains(&resource))
            })
        {
            return Err("a local resource alias needs reconciliation");
        }
        Ok(())
    }

    fn local_views_for(&self, projection: ViewProjection<'_>) -> LocalViews {
        match projection {
            ViewProjection::All => self.local_views(),
            ViewProjection::Structural(scopes) => self.local_views_in(Some(scopes)),
            ViewProjection::Selected(scopes) => {
                #[cfg(test)]
                self.shared_views
                    .selected_projection_count
                    .set(self.shared_views.selected_projection_count.get() + 1);
                let cards = scopes
                    .iter()
                    .map(|scope| (scope.clone(), self.local_view_cards(scope)))
                    .collect();
                let panes = scopes
                    .iter()
                    .map(|scope| (scope.clone(), self.local_view_panes(scope)))
                    .collect();
                LocalViews {
                    scopes: scopes.to_vec(),
                    cards,
                    panes,
                    settled: HashSet::new(),
                }
            }
        }
    }

    fn pane_move_lifecycle_pending(&self, card: CardId) -> bool {
        self.pane_close_owned(card)
            || self.terminal_start_in_flight(card)
            || self.chat_start_pending(card)
            || self.reopen_pending(card)
    }

    fn pane_transfer_source(&self, card: CardId) -> Option<(&View, &Path)> {
        if self.pane_move_lifecycle_pending(card)
            || self.focused_repository_is_saving()
            || matches!(self.cards.get(&card), Some(CardRuntime::Repository(runtime)) if runtime.is_saving())
        {
            return None;
        }
        if !self.card_state(card).is_some_and(shows_in_worktree)
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
        if self.shared_views.quarantined.contains(&card) {
            return None;
        }
        let source = self
            .workspace_model
            .views
            .iter()
            .find(|view| view.panes.iter().any(|pane| pane.id == card))?;
        if self.shared_views.blocked.contains(&source.scope)
            || self.shared_views.seed_failures.contains_key(&source.scope)
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
            && !self
                .shared_views
                .seed_failures
                .contains_key(&Scope::Channel(destination.id))
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
        if self.shared_views.quarantined.contains(&card) {
            self.status = Some(TransientNotice::warning(
                "This pane can't move to another channel because cagents can't confirm its saved session. Close it instead.",
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
        if let Err(error) = self.workspace_model_tx.try_send(
            ModelEdit::ChangeView {
                scope,
                expected_revision,
                operation,
                change,
            }
            .into(),
        ) {
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
        let baseline = self.shared_views.baseline.entry_or_default(scope.clone());
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
        if self.shared_views.closed.contains(&card) {
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

    /// Whether the first message of `card`, a chat created in `scope`, may
    /// start its provider. The daemon starts a shared pane's first message
    /// only once the registry lists that pane, and a scope still loading,
    /// held back or refused has not published it. Its start waits for the
    /// reconcile that admits the scope (see `release_admitted_first_sends`).
    pub(super) fn first_send_admission(&self, card: CardId, scope: &Scope) -> FirstSendAdmission {
        // Without a registry there is no shared view to wait for.
        if self.shared_views.registry_unavailable {
            return FirstSendAdmission::Admitted;
        }
        // A retried seed keeps its refusal's reason until its receipt.
        if self.shared_views.seed_failures.contains_key(scope)
            && !self.shared_views.seeding.contains(scope)
        {
            return FirstSendAdmission::Refused;
        }
        let published = self.shared_views.attached.contains(&card) || self.listed_in_registry(card);
        if self.shared_registry_connecting()
            || !self.shared_views.initialized.contains(scope)
            || self.shared_views.seeding.contains(scope)
            || self.shared_views.blocked.contains(scope)
            || matches!(scope, Scope::Channel(id) if self.channel_ui.was_deleted(*id) || self.channel_ui.is_deleting(*id))
            || !published
        {
            return FirstSendAdmission::Waiting;
        }
        FirstSendAdmission::Admitted
    }

    /// Whether an open chat has work but no shared descriptor yet, which the
    /// next complete reconciliation publishes.
    pub(super) fn holds_unpublished_local_chat(&self) -> bool {
        self.durable.cards.iter().any(|card| {
            card.existence.is_open()
                && matches!(card.kind, CardKind::Chat(_))
                && !self.shared_views.attached.contains(&card.id)
                && !self.shared_views.retained.contains(&card.id)
                && !self.chat_is_unused_default(card.id)
                && !self.reopen_pending(card.id)
        })
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
                    || self.reopen_pending(card.id)
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
        let projected = self.kept_local_views();
        self.publish_projected_view_changes(projected, ViewProjection::All);
    }

    /// Builds the discovery complete passes keep, as the first one would.
    #[cfg(test)]
    pub(super) fn prime_view_discovery(&mut self) {
        drop(self.kept_local_views());
    }

    /// [`Self::local_views`] through the discovery kept between passes, with
    /// each seeded scope whose panes equal its baseline settled.
    fn kept_local_views(&mut self) -> LocalViews {
        let mut discovery = std::mem::take(&mut self.shared_views.discovery);
        let settlement = discovery::Settlement {
            initialized: self.shared_views.initialized.take_changes(),
            baseline: self.shared_views.baseline.take_changes(),
        };
        let projected = self.discovered_local_views(&mut discovery, settlement);
        self.shared_views.discovery = discovery;
        projected
    }

    /// Takes what a reconciliation projected as the registry's view of each
    /// of `projected_scopes`, then publishes `projected` against that.
    fn publish_reconciled_views(
        &mut self,
        projected: LocalViews,
        projection: ViewProjection<'_>,
        projected_scopes: Vec<Scope>,
    ) {
        for scope in projected_scopes {
            // Its baseline already lists exactly its panes, none of them a
            // chat yet to be published.
            if projected.settled.contains(&scope) {
                continue;
            }
            // The view this pass projected is what the registry holds. A
            // chat yet to be published is not in it, so the publication
            // below inserts it rather than taking it as already shared.
            let mut panes = projected.panes.get(&scope).cloned().unwrap_or_default();
            panes.retain(|pane| !self.unpublished_chat(pane.id));
            self.shared_views.baseline.insert(scope, panes);
        }
        self.publish_projected_view_changes(projected, projection);
    }

    fn publish_projected_view_changes(
        &mut self,
        projected: LocalViews,
        projection: ViewProjection<'_>,
    ) {
        if self.shared_views.applying
            || self.workspace_model.identity == crate::workspace_model::NO_REGISTRY
        {
            return;
        }
        let mut phase = WorkspaceProjectionTiming::new(matches!(projection, ViewProjection::All));
        let LocalViews {
            scopes,
            cards,
            mut panes,
            mut settled,
        } = projected;
        // Ownership is global even when only one destination is published: a
        // transfer's source can be outside the projected scopes.
        let previous_owners = self
            .shared_views
            .baseline
            .iter()
            .flat_map(|(scope, panes)| panes.iter().map(|pane| (pane.id, scope.clone())))
            .collect::<HashMap<_, _>>();
        // A settled scope's panes are its baseline. It publishes nothing
        // unless a transfer reads or writes that baseline: a pane in it that
        // another baseline also lists, or a pane leaving it for another
        // scope. Such a scope is published from its panes as any other.
        if !settled.is_empty() {
            let mut moving = HashSet::new();
            let desired = panes.iter().chain(
                settled
                    .iter()
                    .map(|scope| (scope, &self.shared_views.baseline[scope])),
            );
            for (destination, desired) in desired {
                for pane in desired {
                    if let Some(source) = previous_owners
                        .get(&pane.id)
                        .filter(|source| *source != destination)
                    {
                        moving.insert(source.clone());
                        moving.insert(destination.clone());
                    }
                }
            }
            for scope in moving {
                if settled.remove(&scope) {
                    #[cfg(test)]
                    {
                        self.shared_views.transfer_unsettled += 1;
                    }
                    panes.insert(scope.clone(), self.shared_views.baseline[&scope].clone());
                }
            }
        }
        #[cfg(test)]
        {
            self.shared_views.settled_publications += settled.len();
        }
        phase.mark("publish_owners");
        // Only the projected scopes and the sources their panes may leave are
        // published, so only their canonical views are looked up: a complete
        // pass projects what the window shows, not every historical scope.
        let mut needed = scopes.iter().collect::<HashSet<_>>();
        // A settled scope's panes are in its own baseline, so they name no
        // other source.
        for pane in panes.values().flatten() {
            if let Some(source) = previous_owners.get(&pane.id) {
                needed.insert(source);
            }
        }
        // A stable presentation or seed acknowledgment needs only a few
        // equality lookups. Other passes can span many scopes, so index their
        // required destinations and transfer sources in one model pass. Both
        // preserve the first canonical view for each scope.
        let mut view_indices = HashMap::new();
        if matches!(projection, ViewProjection::Selected(_)) {
            for scope in needed {
                if let Some(index) = self.workspace_model.views.iter().position(|view| {
                    #[cfg(test)]
                    {
                        self.shared_views.publisher_scope_comparisons += 1;
                    }
                    &view.scope == scope
                }) {
                    #[cfg(test)]
                    {
                        self.shared_views.publisher_index_entries += 1;
                    }
                    view_indices.insert(scope.clone(), index);
                }
            }
        } else {
            let index = self
                .shared_views
                .view_index
                .refresh(&self.workspace_model.views);
            for scope in needed {
                if let Some(&index) = index.get(scope) {
                    #[cfg(test)]
                    {
                        self.shared_views.publisher_index_entries += 1;
                    }
                    view_indices.insert(scope.clone(), index);
                }
            }
        }
        phase.mark("publish_indices");
        // A quarantined pane keeps its canonical descriptor and placement:
        // this client neither seeds, moves nor rewrites it. Closing it is the
        // one change it publishes, so it leaves the view like any closed pane.
        let quarantined = self.fenced_panes();
        for (destination, desired) in &panes {
            for (index, pane) in desired.iter().enumerate() {
                if let Some(source) = previous_owners
                    .get(&pane.id)
                    .cloned()
                    .filter(|source| source != destination)
                {
                    if !self.shared_views.initialized.contains(&source)
                        || quarantined.contains(&pane.id)
                        || self.shared_views.blocked.contains(&source)
                        || self.shared_views.blocked.contains(destination)
                        || self.shared_views.seed_failures.contains_key(&source)
                        || self.shared_views.seed_failures.contains_key(destination)
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
                        .entry_or_default(destination.clone());
                    baseline.insert(index.min(baseline.len()), pane.clone());
                }
            }
        }
        phase.mark("publish_transfers");
        // A fenced pane the reader closed leaves the view that holds it,
        // wherever this window showed it. Its saved source is unconfirmed, so
        // the close names the pane rather than a source the registry would
        // refuse, as a seed's closed list does. Until a scope has seeded, its
        // seed carries the close instead; a refused removal is derived again.
        let mut closed_fenced = Vec::new();
        if !self.shared_views.fenced_closes.is_empty() {
            let mut held = HashSet::new();
            for view in &self.workspace_model.views {
                for pane in &view.panes {
                    if !self.shared_views.fenced_closes.contains(&pane.id) {
                        continue;
                    }
                    held.insert(pane.id);
                    if self.shared_views.initialized.contains(&view.scope)
                        && self
                            .card_state(pane.id)
                            .is_some_and(|card| !card.existence.is_open())
                        && !self.shared_views.pending.iter().any(|(_, _, change)| {
                            matches!(change, Change::Remove { id } if *id == pane.id)
                        })
                    {
                        closed_fenced.push((view.scope.clone(), view.revision, pane.id));
                    }
                }
            }
            // Once no view holds it, the removal has landed.
            self.shared_views
                .fenced_closes
                .retain(|id| held.contains(id));
        }
        for (scope, expected_revision, id) in closed_fenced {
            let operation = uuid::Uuid::new_v4();
            let change = Change::Remove { id };
            self.shared_views
                .pending
                .push((operation, scope.clone(), change.clone()));
            self.publish_model_edit(ModelEdit::ChangeView {
                scope,
                expected_revision,
                operation,
                change,
            });
        }
        for scope in scopes {
            if self.shared_views.blocked.contains(&scope)
                || self.shared_views.seed_failures.contains_key(&scope)
            {
                continue;
            }
            if matches!(&scope, Scope::Channel(id) if self.channel_ui.was_deleted(*id) || self.channel_ui.is_deleting(*id))
            {
                continue;
            }
            let settled = settled.contains(&scope);
            let panes = panes.remove(&scope).unwrap_or_default();
            if !settled && !self.shared_views.initialized.contains(&scope) {
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
                        .filter(|pane| {
                            !quarantined.contains(&pane.id)
                                && !self.durable.pending_terminal_creations.contains(&pane.id)
                        })
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
            let revision = view_indices
                .get(&scope)
                .map(|&index| self.workspace_model.views[index].revision)
                .unwrap_or(0);
            // A settled scope's panes equal its baseline: no change, and its
            // baseline stays as it is.
            let changes = if settled {
                Vec::new()
            } else {
                let previous = self
                    .shared_views
                    .baseline
                    .get(&scope)
                    .map_or(&[][..], Vec::as_slice);
                if quarantined.is_empty() {
                    views::changes(previous, &panes)
                } else {
                    let unfenced = |panes: &[Pane]| {
                        panes
                            .iter()
                            .filter(|pane| !quarantined.contains(&pane.id))
                            .cloned()
                            .collect::<Vec<_>>()
                    };
                    views::changes(&unfenced(previous), &unfenced(&panes))
                }
            };
            for change in changes {
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
                let change = if let Change::Remove { id } = &change {
                    // Never turn a placement diff into source-free shared Close.
                    let Some(change) = self.shared_removal_change(*id, &scope) else {
                        continue;
                    };
                    change
                } else {
                    change
                };
                let change = match change {
                    Change::Update { before, mut pane }
                        if matches!((&before.kind, &pane.kind),
                            (CardKind::Terminal(a), CardKind::Terminal(b)) if a.id == b.id && a.launch == b.launch)
                            && before.kind != pane.kind =>
                    {
                        let worker = match self.cards.get(&pane.id) {
                            Some(CardRuntime::Terminal(handle)) => handle.execution_instance().zip(handle.host_proof())
                                .map(|(instance, owner)| views::TerminalRemovalBasis::Worker { owner, instance }),
                            _ => self.shared_views.terminal_source_workers.get(&pane.id)
                                .filter(|(id, _)| matches!(&pane.kind, CardKind::Terminal(s) if &s.id == id))
                                .map(|(_, worker)| worker.clone()),
                        };
                        if let Some(worker) = worker {
                            Change::UpdateTerminalSource {
                                before,
                                pane,
                                worker,
                            }
                        } else {
                            // A source without a surface receipt is saved locally,
                            // but cannot overwrite another worker's canonical state.
                            pane.kind = before.kind.clone();
                            Change::Update { before, pane }
                        }
                    }
                    change => change,
                };
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
            if !settled {
                self.shared_views.baseline.insert(scope.clone(), panes);
            }
            let defaults = cards[&scope]
                .iter()
                .copied()
                .filter(|id| self.shared_views.defaults.contains_key(id))
                .collect::<Vec<_>>();
            for id in defaults {
                if self.shared_views.default_requests.insert(id)
                    && let Some(mut pane) = self.card_state(id).and_then(|card| {
                        Pane::from_card(card, self.durable.card_scopes.get(&id).cloned())
                    })
                {
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
        phase.mark("publish_scopes");
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
            self.shared_views.seed_attempts.remove(&scope);
            self.shared_views.seed_failures.remove(&scope);
            self.shared_views.initialized.insert(scope.clone());
            // The receipt admits only this scope. Other scopes retain their
            // own seed/baseline and pending edits until their receipts arrive.
            let cards = self.local_view_cards(&scope);
            let panes = self.local_view_panes(&scope);
            self.rebase_seeded_panes(&scope, &panes);
            self.publish_projected_view_changes(
                LocalViews {
                    scopes: vec![scope.clone()],
                    cards: HashMap::from([(scope.clone(), cards)]),
                    panes: HashMap::from([(scope.clone(), panes)]),
                    settled: HashSet::new(),
                },
                ViewProjection::Selected(&[scope]),
            );
        }
    }

    /// A seed's baseline is what this window saved, but the registry keeps the
    /// panes it already holds. Two of this window's differences from such a
    /// pane are based on the registry's record instead, so they are published:
    ///
    /// - A terminal attached while the seed was in flight has proved its same
    ///   session under the running daemon. Based on the saved record the
    ///   update is refused, and the record then stays behind the attachment
    ///   until another window writes.
    /// - A pane name the registry's pane lacks never reached it, as one given
    ///   before the registry existed or whose publication failed: a person can
    ///   change a pane's name but not remove it. Unpublished, the registry's
    ///   pane differs from this window's on every start, which migrates the
    ///   view again (#3032). A name the registry holds is newer and stays.
    fn rebase_seeded_panes(&mut self, scope: &Scope, local: &[Pane]) {
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
            if shared.user_title.is_none() && seeded.user_title == current.user_title {
                seeded.user_title = None;
            }
        }
    }

    /// Fenced cards for the model this client holds now. Installing a
    /// snapshot adds the panes it fences; projection recomputes the set with
    /// their local aliases.
    fn fenced_panes(&self) -> HashSet<CardId> {
        self.shared_views.quarantined.clone()
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
        let fenced = self.fenced_panes();
        for (operation, scope, change) in &mut self.shared_views.pending {
            let (Change::Update { before, pane }
            | Change::UpdateTerminalSource { before, pane, .. }) = change
            else {
                continue;
            };
            let card = pane.id;
            if !moving.contains(&card) {
                continue;
            }
            if fenced.contains(&card) {
                // A fenced pane's owner cannot be verified; nothing retries.
                discarded.insert(*operation);
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
            if let Change::UpdateTerminalSource { before, pane, .. } = change
                && !views::updates_source(before, pane)
            {
                *change = Change::Update {
                    before: before.clone(),
                    pane: pane.clone(),
                };
            }
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

    #[cfg(test)]
    /// A full canonical snapshot already supplies view authority. A seed is
    /// needed only when local migration could change that view, not as a
    /// per-client handshake. Keep exact membership and field equality here:
    /// omitted defaults/retained cards and uncommitted intentions must still
    /// take the ordinary migration path.
    fn admit_existing_views_without_migration(&mut self) {
        self.admit_existing_views_in(None);
    }

    #[cfg(test)]
    fn admit_existing_views_in(&mut self, selected: Option<&HashSet<Scope>>) {
        self.admit_existing_views_with_topology(selected, None);
    }

    fn admit_existing_views_with_topology(
        &mut self,
        selected: Option<&HashSet<Scope>>,
        topology: Option<&ViewScopeTopology<'_>>,
    ) {
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
                    && !self.shared_views.seed_failures.contains_key(&view.scope)
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
        // Eligibility reads the mounted order without taking a durable save
        // snapshot. A repository's admission must not snapshot another scope
        // or scan every saved card to normalize unrelated channel membership.
        let mut projected = match topology {
            Some(topology) => self.local_views_with_topology(selected, topology),
            None => self.local_views_in(selected),
        };
        if self.active_workspace.is_some()
            && self.durable.active_channel.is_some_and(|id| {
                selected.is_none_or(|selected| selected.contains(&Scope::Channel(id)))
            })
            && let Some((id, mut order)) = self.active_channel_order()
        {
            let scope = Scope::Channel(id);
            if let Some(members) = projected.cards.get_mut(&scope) {
                let open = members.iter().copied().collect::<HashSet<_>>();
                order.retain(|id| open.contains(id));
                let positions = order
                    .iter()
                    .enumerate()
                    .map(|(index, id)| (*id, index))
                    .collect::<HashMap<_, _>>();
                projected
                    .panes
                    .get_mut(&scope)
                    .unwrap()
                    .sort_by_key(|pane| positions[&pane.id]);
                *members = order;
            }
        }
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

    #[cfg(test)]
    /// A repository load changes ownership boundaries, not registry authority.
    /// Follow pane ownership across local, baseline and canonical scopes so a
    /// discovered child root, transfer or rekeyed alias settles both ends.
    fn repository_view_scopes(
        &self,
        repository: &Path,
        additional: &HashSet<Scope>,
    ) -> HashSet<Scope> {
        let topology = self.borrowed_view_scope_topology();
        self.repository_view_scopes_with_topology(&[repository], additional, &topology)
    }

    /// The scopes [`Self::repository_view_scopes`] selects for each of
    /// `repositories`, followed through ownership together.
    fn repository_view_scopes_with_topology(
        &self,
        repositories: &[&Path],
        additional: &HashSet<Scope>,
        topology: &ViewScopeTopology<'_>,
    ) -> HashSet<Scope> {
        let discovery = self.discover_view_scopes_with(topology);
        let repositories = repositories.iter().copied().collect::<HashSet<_>>();
        let projects = self
            .sidebar_model
            .projects
            .iter()
            .filter(|project| repositories.contains(project.repository.as_path()))
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
                    .filter(|channel| repositories.contains(channel.repository.as_path()))
                    .map(|channel| channel.id),
            )
            .collect::<HashSet<_>>();
        let belongs = |scope: &Scope| match scope {
            Scope::Worktree {
                repository: owner, ..
            } => repositories.contains(owner.as_path()),
            Scope::Channel(id) => channels.contains(id),
        };
        let mut selected = discovery
            .scopes
            .iter()
            .map(|scope| &**scope)
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
        for scope in discovery.scopes.iter().map(|scope| &**scope) {
            if let Scope::Worktree { path, .. } = scope {
                scopes_by_path
                    .entry(path.as_path())
                    .or_default()
                    .push(scope);
            }
        }
        for (card, path) in discovery.worktree_cards {
            if shows_in_worktree(card)
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
        let alias_candidates = unowned_aliases(self.durable.cards.iter(), &owned);
        for view in &self.workspace_model.views {
            for pane in &view.panes {
                owners
                    .entry(pane.id)
                    .or_default()
                    .insert(view.scope.clone());
                // A canonical rekey can retire an unowned local alias in a
                // different scope; its source baseline must settle too.
                for alias in projection_resource(&pane.kind)
                    .and_then(|resource| alias_candidates.get(&resource))
                    .into_iter()
                    .flatten()
                {
                    let aliases = owners.get(&alias.id).cloned().unwrap_or_default();
                    owners.entry(pane.id).or_default().extend(aliases);
                }
            }
        }
        for (_, scope, change) in &self.shared_views.pending {
            let entries = owners.entry(change.pane_id()).or_default();
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

    /// Called only after the presentation-only proof has established unchanged
    /// global ownership, settled local membership and no admission obligations.
    pub(super) fn reconcile_shared_presentation(&mut self, scopes: &[Scope]) -> Task<Message> {
        // Channel membership is an explicit local row. Worktree membership is
        // still derived from root discovery and requires the complete pass.
        if scopes
            .iter()
            .any(|scope| !matches!(scope, Scope::Channel(_)))
        {
            return self.reconcile_shared_views();
        }
        self.reconcile_shared_views_with(ViewProjection::Selected(scopes), None)
    }

    #[cfg(test)]
    pub(super) fn reconcile_shared_views_for_repository(
        &mut self,
        repository: &Path,
        adopted: &HashSet<Scope>,
    ) -> Task<Message> {
        self.reconcile_shared_views_for_repositories(&[repository], adopted)
    }

    /// One structural pass over the scopes of every repository in
    /// `repositories`, as each would select alone.
    pub(super) fn reconcile_shared_views_for_repositories(
        &mut self,
        repositories: &[&Path],
        adopted: &HashSet<Scope>,
    ) -> Task<Message> {
        self.reconcile_shared_views_in(Some((repositories, adopted)))
    }

    fn reconcile_shared_views_in(
        &mut self,
        repository: Option<(&[&Path], &HashSet<Scope>)>,
    ) -> Task<Message> {
        self.publish_unpublished_local_chats();
        // A watcher snapshot can announce the destination before the explicit
        // transfer response arrives. Rebase before selecting either owner.
        self.retarget_transferred_pane_updates(None);
        #[cfg(test)]
        self.shared_views
            .structural_pass_count
            .set(self.shared_views.structural_pass_count.get() + 1);
        // Roots and repository authority stay fixed for this whole pass, so
        // admission, alias repair, projection and release share one index.
        // The one discovery keeps, built again only when the projects or
        // the known roots changed; the save after this pass refreshes the same.
        let mut phase = WorkspaceProjectionTiming::new(true);
        let kept = self.kept_view_topology();
        let topology = kept.scope_topology();
        phase.mark("shared_topology");
        let selected = repository.map(|(repository, adopted)| {
            self.repository_view_scopes_with_topology(repository, adopted, &topology)
        });
        self.reconcile_shared_views_with(
            selected
                .as_ref()
                .map_or(ViewProjection::All, ViewProjection::Structural),
            Some(&topology),
        )
    }

    /// A structural pass reads roots and authority from its caller's
    /// `topology`; a presentation-only pass needs none.
    fn reconcile_shared_views_with(
        &mut self,
        projection: ViewProjection<'_>,
        topology: Option<&ViewScopeTopology<'_>>,
    ) -> Task<Message> {
        // Capture before alias or canonical removal can detach a pane. All
        // views in this snapshot share one final camera/visibility projection.
        let viewport = self.workspace.capture_viewport_anchor();
        let presentation = matches!(projection, ViewProjection::Selected(_));
        debug_assert_eq!(
            topology.is_none(),
            presentation,
            "only a structural pass shares its caller's topology"
        );
        let selected = match projection {
            ViewProjection::Structural(scopes) => Some(scopes),
            _ => None,
        };
        // A selected pass reports its own stages; a structural pass reports
        // finer ones, since it is the one every changing edge pays.
        let mut phase = WorkspaceProjectionTiming::new(true);
        let stage = |selected: &'static str, structural: &'static str| {
            if presentation { selected } else { structural }
        };
        if !presentation {
            self.shared_views.prune_restoration_requests(&self.durable);
            self.admit_existing_views_with_topology(selected, topology);
            phase.mark("shared_admit");
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
            phase.mark("shared_observe");
        } else if let ViewProjection::Selected(scopes) = projection {
            // Preserve the same raw source evidence the complete pass records
            // for a future close, without scanning unrelated local history.
            for pane in self
                .workspace_model
                .views
                .iter()
                .filter(|view| scopes.contains(&view.scope))
                .flat_map(|view| &view.panes)
            {
                if let CardKind::Chat(session) = &pane.kind {
                    self.shared_views
                        .observed_chats
                        .insert(pane.id, session.clone());
                } else {
                    self.shared_views.observed_chats.remove(&pane.id);
                }
            }
        }
        self.shared_views.applying = true;
        let mut tasks = Vec::new();
        // The first authoritative snapshot of an original scope still belongs
        // to startup even if the gate already reported incomplete recovery.
        // Capture before optimistic edits or alias projection add local panes.
        // A selected pass changed only its scopes; the rest were seen before.
        let mut views = match projection {
            ViewProjection::All | ViewProjection::Structural(_) => {
                self.workspace_model.views.clone()
            }
            ViewProjection::Selected(scopes) => self
                .workspace_model
                .views
                .iter()
                .filter(|view| scopes.contains(&view.scope))
                .cloned()
                .collect(),
        };
        for view in &views {
            self.startup_gate
                .initial_panes
                .observe_initial_view(view, &self.durable.cards);
        }
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
        // Each pending edit is overlaid exactly once. One that no longer fits
        // the observed view, such as a Close of a pane its agent has already
        // retired, is left out: the registry answers every pending edit, and
        // that answer is what the person is told about if anything was lost.
        for view in &mut views {
            for change in pending_by_scope.get(&view.scope).into_iter().flatten() {
                if let Err(error) = view.apply(change) {
                    tracing::debug!(scope = ?view.scope, %error, "a pending pane edit no longer fits the observed view");
                }
            }
        }
        if !presentation {
            phase.mark("shared_overlay");
        }
        // A scoped read or a deferred receipt may prove a close beyond the
        // snapshot that supplied a descriptor. Exact closed identity wins over
        // both that older descriptor and a pending local insertion overlay.
        // So does this window's own close: a card it closed stays closed
        // under its identity, whatever a view still lists. Storage holds its
        // tombstone and would refuse it reopened (#2093, #2444).
        self.index_cards();
        for view in &mut views {
            let closed: Vec<_> = view
                .panes
                .iter()
                .filter(|pane| {
                    self.shared_views.closed.contains(&pane.id) || self.closed_here(pane.id)
                })
                .map(|pane| pane.id)
                .collect();
            if closed.is_empty() {
                continue;
            }
            view.panes.retain(|pane| !closed.contains(&pane.id));
            for id in closed {
                if !view.retired.contains(&id) {
                    view.retired.push(id);
                }
            }
        }
        let owned = if !presentation {
            views
                .iter()
                .flat_map(|view| view.panes.iter().map(|pane| pane.id))
                .collect::<HashSet<_>>()
        } else {
            self.workspace_model
                .views
                .iter()
                .flat_map(|view| view.panes.iter().map(|pane| pane.id))
                .collect::<HashSet<_>>()
        };
        if let Some(topology) = topology {
            self.prepare_shared_aliases_in(&views, &owned, selected, topology);
        }
        if !presentation {
            phase.mark("shared_aliases");
        }
        // Root discovery is immutable during this projection, so index it once
        // over owned paths that outlive each mutable view update. Keep only
        // owned card paths too, and refresh the panes each view changes before
        // visiting the next one.
        let unindexed;
        let roots = match topology {
            Some(topology) => &topology.roots,
            None => {
                unindexed = worktree_ui::WorktreeRoots::new(std::iter::empty());
                &unindexed
            }
        };
        let mut local_worktrees = CardWorktrees::default();
        if !presentation {
            for card in self
                .durable
                .cards
                .iter()
                .filter(|card| card.existence.is_open())
            {
                if let Some(path) = self.unchanneled_card_worktree_indexed(card, roots) {
                    local_worktrees.insert(card.id, path.to_owned());
                }
            }
        }
        phase.mark(stage("selected_prepare", "shared_worktrees"));
        // Whether the registry lists a pane, read once for every pane this
        // pass projects rather than by a walk of every view for each.
        let listed = self.registry_listed_panes();
        let mut projected_scopes = Vec::new();
        for view in views {
            if selected.is_some_and(|selected| !selected.contains(&view.scope)) {
                continue;
            }
            if self.shared_views.blocked.contains(&view.scope) {
                continue;
            }
            if !self.shared_views.initialized.contains(&view.scope)
                || self.shared_views.seeding.contains(&view.scope)
                || self.shared_views.seed_failures.contains_key(&view.scope)
            {
                continue;
            }
            let current = match &view.scope {
                Scope::Channel(_) => self.local_view_cards(&view.scope),
                // Most views are historical: no card resolves to them at all.
                Scope::Worktree { path, .. } if !local_worktrees.holds(path) => Vec::new(),
                Scope::Worktree { path, .. } => self.worktree_strip_cards(&local_worktrees, path),
            };
            #[cfg(test)]
            self.shared_views
                .view_projection_count
                .set(self.shared_views.view_projection_count.get() + 1);
            // A historical worktree view, with no pane listed and no card
            // resolving to it, projects nothing unless it is the active strip.
            let historical = match &view.scope {
                Scope::Worktree { path, .. } => {
                    current.is_empty()
                        && view.panes.is_empty()
                        && !(self.durable.active_channel.is_none()
                            && self.active_workspace.as_ref() == Some(path))
                }
                Scope::Channel(_) => false,
            };
            if historical {
                projected_scopes.push(view.scope);
                continue;
            }
            tasks.push(self.project_shared_view_with_cards(&view, &owned, &listed, current));
            if !view.panes.is_empty() {
                for pane in &view.panes {
                    if let Some(path) = self
                        .card_state(pane.id)
                        .and_then(|card| self.unchanneled_card_worktree_indexed(card, roots))
                    {
                        local_worktrees.insert(pane.id, path.to_owned());
                    } else {
                        local_worktrees.remove(pane.id);
                    }
                }
            }
            projected_scopes.push(view.scope);
        }
        if !presentation {
            phase.mark("shared_project");
        }
        tasks.push(self.finish_shared_viewport_projection(viewport));
        phase.mark(stage("selected_project", "shared_viewport"));
        self.shared_views.applying = false;
        // Capture after the active strip snapshot, then share this immutable
        // pass between settled baselines and publication. Uninitialized scopes
        // and local/default intentions still use the ordinary publisher.
        // Held reported titles stay held: publishing them on every view
        // acknowledgment would publish each title again as its own echo.
        self.save_projected_workspace(presentation, |app| {
            // A complete pass discovers through what the last pass kept and
            // settles the scopes whose panes it left unchanged, rather than
            // copying and comparing every scope's panes again.
            let projected = match (projection, topology) {
                (ViewProjection::All, Some(_)) => app.kept_local_views(),
                (ViewProjection::Structural(scopes), Some(topology)) => {
                    app.local_views_with_topology(Some(scopes), topology)
                }
                _ => app.local_views_for(projection),
            };
            app.publish_reconciled_views(projected, projection, projected_scopes);
        });
        phase.mark(stage("selected_save", "shared_save"));
        tasks.push(self.run_deferred_default_pane());
        // A closed pane a view brought back after its conversation stopped
        // goes again, now that the views it is removed from are settled.
        tasks.push(self.remove_stopped_returns());
        // A closed pane a view put back while its conversation stops stays
        // out of sight, now that the strip holds it.
        tasks.push(self.hide_returned_closes());
        // Presentation-only projection preserves every notification attribution
        // input: source, membership, ownership, placement and project identity.
        // Journal snapshots attribute their own new evidence independently.
        if !presentation {
            phase.mark("shared_deferred");
            self.reproject_notification_unread();
            phase.mark("shared_unread");
        }
        tasks.push(self.release_held_terminals_with_topology(topology));
        phase.mark(stage("selected_terminals", "shared_terminals"));
        // Every change to a scope's admission (a seed receipt or refusal, or
        // the blocked set rebuilt above) is followed by this pass, after the
        // pane's Insert was queued above.
        tasks.push(self.release_admitted_first_sends());
        phase.mark(stage("selected_first_sends", "shared_first_sends"));
        // Restored shared panes queued for an attach wait for the registry.
        tasks.push(self.attach_next_restored_agent());
        phase.mark(stage("selected_postlude", "shared_postlude"));
        Task::batch(tasks)
    }

    #[cfg(test)]
    fn prepare_shared_aliases(&mut self, views: &[View], owned: &HashSet<CardId>) {
        let kept = self.kept_view_topology();
        self.prepare_shared_aliases_in(views, owned, None, &kept.scope_topology());
        // Outside a reconciliation, no projection follows to take the
        // detached aliases out of the strip.
        self.remove_detached_from_strip();
    }

    fn prepare_shared_aliases_in(
        &mut self,
        views: &[View],
        owned: &HashSet<CardId>,
        selected: Option<&HashSet<Scope>>,
        topology: &ViewScopeTopology<'_>,
    ) {
        self.shared_views.blocked.clear();
        let display_index = views::ManagedDisplayIndex::new(&self.workspace_model);
        self.shared_views.quarantined =
            quarantined_managed_panes(&self.workspace_model, views, &display_index);
        let mut attention_owners: HashMap<_, usize> = HashMap::new();
        for pane in views.iter().flat_map(|view| &view.panes) {
            if let Some(resource) = attention_resource(&pane.kind) {
                *attention_owners.entry(resource).or_default() += 1;
            }
        }
        // Decide which whole views can project before retiring any local alias.
        // Otherwise a later blocked pane could leave an earlier adoption without
        // the canonical descriptor required by its persistence checkpoint.
        let alias_candidates = unowned_aliases(self.durable.cards.iter(), owned);
        let mut adoptions = Vec::new();
        for view in views {
            for shared in &view.panes {
                let aliases = projection_resource(&shared.kind)
                    .and_then(|resource| alias_candidates.get(&resource))
                    .into_iter()
                    .flatten()
                    .map(|card| card.id)
                    .collect::<Vec<_>>();
                if self.shared_views.quarantined.contains(&shared.id) {
                    // A fenced canonical pane cannot replace its local alias.
                    self.shared_views.quarantined.extend(aliases);
                    continue;
                }
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
                        for scope in self.local_scopes_listing_with_topology(alias, topology) {
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
                        for scope in self.local_scopes_listing_with_topology(alias, topology) {
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
            if matches!(shared.kind, CardKind::Chat(_)) {
                self.carry_draft_model(alias, shared.id);
            }
            self.detach_shared_pane(alias);
            self.remove_shared_channel_member(alias);
        }
    }

    /// Removing an alias or moving a pane to a worktree must leave a valid
    /// saved channel even when its now-empty inactive view needs no projection.
    fn remove_shared_channel_member(&mut self, id: CardId) {
        if !self
            .durable
            .channels
            .iter()
            .any(|channel| channel.focused == Some(id) || channel.cards.contains(&id))
        {
            return;
        }
        for channel in &mut Arc::make_mut(&mut self.durable).channels {
            channel.cards.retain(|card| *card != id);
            if channel.focused == Some(id) {
                channel.focused = channel.cards.first().copied();
            }
        }
    }

    fn prepare_terminal_restoration(&mut self, pane: &Pane) {
        let CardKind::Terminal(terminal) = &pane.kind else {
            return;
        };
        let already_projected = self.card_state(pane.id).is_some_and(|card| {
            card.existence.is_open()
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

    #[cfg(test)]
    fn project_shared_view(&mut self, view: &View, owned: &HashSet<CardId>) -> Task<Message> {
        let viewport = self.workspace.capture_viewport_anchor();
        let current = self.local_view_cards(&view.scope);
        let listed = self.registry_listed_panes();
        let projection = self.project_shared_view_with_cards(view, owned, &listed, current);
        Task::batch([projection, self.finish_shared_viewport_projection(viewport)])
    }

    /// Whether `id` is a chat this window has yet to share with `view`.
    ///
    /// Such a chat is not missing from the view: its Insert has yet to be
    /// published, as when the previous pass held its scope back and this one
    /// admits it. It keeps its place here, and the publication that follows
    /// the projection inserts it. Kept as a local-only pane instead, it would
    /// never be published, and its first message would start unrecorded
    /// (#2697).
    fn awaits_first_publication(&self, id: CardId, view: &View) -> bool {
        self.unpublished_chat(id) && !view.retired.contains(&id)
    }

    /// Whether `id` is a chat this window has neither shared nor kept local,
    /// and will share. An untouched automatic composer is never shared: it
    /// gives way to what the view holds, as the projection decides.
    fn unpublished_chat(&self, id: CardId) -> bool {
        !self.shared_views.attached.contains(&id)
            && !self.shared_views.retained.contains(&id)
            && !self.chat_is_unused_default(id)
            && self
                .card_state(id)
                .is_some_and(|card| matches!(&card.kind, CardKind::Chat(_)))
    }

    /// The cards a worktree view's strip holds here: open, not a utility page,
    /// in no channel and resolving to `path`, in the workspace's card order.
    /// Only the few cards resolving to `path` are visited, never every card
    /// the workspace has ever opened.
    fn worktree_strip_cards(&self, local_worktrees: &CardWorktrees, path: &Path) -> Vec<CardId> {
        let channel_cards = self
            .durable
            .channels
            .iter()
            .flat_map(|channel| channel.cards.iter().copied())
            .collect::<HashSet<_>>();
        let mut members = local_worktrees
            .cards_in(path)
            .filter(|card| !channel_cards.contains(card))
            .filter_map(|card| {
                let index = self.card_index(card)?;
                shows_in_worktree(&self.durable.cards[index]).then_some((index, card))
            })
            .collect::<Vec<_>>();
        members.sort_unstable_by_key(|&(index, _)| index);
        members.into_iter().map(|(_, card)| card).collect()
    }

    /// `listed` holds every pane the registry's raw views list, which
    /// projecting a view leaves as it is.
    fn project_shared_view_with_cards(
        &mut self,
        view: &View,
        owned: &HashSet<CardId>,
        listed: &HashSet<CardId>,
        mut current: Vec<CardId>,
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
        let mut user_titles_changed = false;
        let mut retained_upgrades = Vec::new();
        let local_order = current.clone();
        let local_members = current.iter().copied().collect::<HashSet<_>>();
        for id in current.iter().copied() {
            if self.chat_is_unused_default(id) && view.panes.is_empty() {
                continue;
            }
            // A pane still reopening is not shared yet, so no view lists it.
            if owned.contains(&id)
                || self.shared_views.quarantined.contains(&id)
                || self.reopen_pending(id)
                || self
                    .card_state(id)
                    .is_some_and(|card| card.kind.is_utility())
            {
                continue;
            }
            if self.awaits_first_publication(id, view) {
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
            if self.shared_views.quarantined.contains(&shared.id) {
                continue;
            }
            self.shared_views.closed_chats.remove(&shared.id);
            self.sync_closed_chat_draft(shared.id);
            self.prepare_terminal_restoration(shared);
            // Most panes project as the view lists them, so one is copied
            // only to change it.
            let mut projected = Cow::Borrowed(shared);
            if let Scope::Worktree { path, .. } = &view.scope {
                if projected.scope.as_ref() != Some(path) {
                    projected.to_mut().scope = Some(path.clone());
                }
                self.worktree_owners.insert(projected.id, path.clone());
                self.remove_shared_channel_member(projected.id);
            }

            if let CardKind::Repository(repository) = &projected.kind {
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
                    && project != repository.project
                    && let CardKind::Repository(repository) = &mut projected.to_mut().kind
                {
                    repository.project = project;
                }
            }
            if let CardKind::Browser(browser) = &projected.kind
                && let Some(CardKind::Browser(local)) =
                    self.card_state(projected.id).map(|card| &card.kind)
                && local.current_url == browser.current_url
                && (local.scroll_x, local.scroll_y) != (browser.scroll_x, browser.scroll_y)
            {
                let (scroll_x, scroll_y) = (local.scroll_x, local.scroll_y);
                if let CardKind::Browser(browser) = &mut projected.to_mut().kind {
                    browser.scroll_x = scroll_x;
                    browser.scroll_y = scroll_y;
                }
            }
            let pane: &Pane = &projected;
            let accepted_default = listed.contains(&pane.id);
            if accepted_default {
                let automatic = self.shared_views.defaults.remove(&pane.id);
                self.shared_views.default_requests.remove(&pane.id);
                let explicit = self.durable.pending_terminal_creations.contains(&pane.id)
                    && Arc::make_mut(&mut self.durable)
                        .pending_terminal_creations
                        .remove(&pane.id);
                if (automatic.is_some() || explicit)
                    && let CardKind::Terminal(session) = &pane.kind
                {
                    self.shared_views.attached.insert(pane.id);
                    tasks.push(self.start_terminal(pane.id, session.clone(), true, false));
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
            let keep_live = self.observe_terminal_save(pane);
            if !keep_live {
                user_titles_changed |=
                    self.chrome_ui.user_titles.get(&pane.id) != pane.user_title.as_ref();
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
            }
            // The card as it stands before anything below changes it. Its
            // kind is copied only when it differs from the pane's.
            let existing = self.card_state(pane.id).map(|card| {
                let kind = if card.kind == pane.kind {
                    Cow::Borrowed(&pane.kind)
                } else {
                    Cow::Owned(card.kind.clone())
                };
                (card.existence.is_open(), kind)
            });
            // The exit this window saw, read before a readmission below can
            // mark the card unstarted.
            let mut observed_exit = self.ended_terminal(pane.id).cloned();
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
            let canonical_was_ended = match &pane.kind {
                CardKind::Terminal(source) => {
                    match self.shared_views.observed_terminals.get(&pane.id) {
                        Some(previous) if previous == source => terminal_ended(previous),
                        _ => self
                            .shared_views
                            .observed_terminals
                            .insert(pane.id, source.clone())
                            .is_none_or(|previous| terminal_ended(&previous)),
                    }
                }
                _ => {
                    self.shared_views.observed_terminals.remove(&pane.id);
                    false
                }
            };
            let completed_became_live = no_start
                && canonical_was_ended
                && observed_exit.is_some()
                && matches!(self.cards.get(&pane.id), Some(CardRuntime::Completed))
                && matches!(&pane.kind, CardKind::Terminal(s) if s.continuity.live_proof().is_some())
                && !pane.pending_terminal_start;
            if completed_became_live {
                if let CardKind::Terminal(s) = &pane.kind {
                    self.retained_outputs.remove(&s.id);
                }
                self.shared_views.terminal_source_workers.remove(&pane.id);
                // The canonical live source attaches; saved exit state cannot
                // turn its replacement back into the old Completed placeholder.
                observed_exit = None;
            }
            let failed_pending = pane.pending_terminal_start
                && matches!(self.cards.get(&pane.id), Some(CardRuntime::Failed(_)));
            if matches!(pane.kind, CardKind::Terminal(_))
                && ((no_start && (waiting || failed_pending || completed_became_live))
                    || (leaves_saved
                        && matches!(self.cards.get(&pane.id), Some(CardRuntime::Loading))))
            {
                let exposure = self.exposure(pane.id);
                self.readmit_held_terminal(pane.id, exposure);
            }
            match existing {
                Some((true, saved)) => {
                    // The daemon can publish the conversation a resume in this
                    // window started before that start replies. The start owns
                    // the card's runtime, with its transcript and draft, and
                    // puts the conversation in the card itself once it is saved.
                    let resuming = matches!(&pane.kind, CardKind::Chat(session) if self.chat_resume_in_flight(pane.id, session));
                    if *saved != pane.kind
                        && matches!(self.cards.get(&pane.id), Some(CardRuntime::Repository(runtime)) if runtime.is_dirty() || runtime.is_saving())
                    {
                        self.status = Some(TransientNotice::warning(
                            "Another client changed this pane. Save or discard its unsaved edits before adopting that change.",
                        ));
                        continue;
                    }
                    if *saved != pane.kind && !resuming {
                        // Runtime metadata can change, but an exact live actor
                        // remains attached unless the session identity changes.
                        let same_session = match (&*saved, &pane.kind) {
                            (CardKind::Terminal(a), CardKind::Terminal(b)) => a.id == b.id,
                            (CardKind::Chat(a), CardKind::Chat(b)) => a.id == b.id,
                            (a, b) => a == b,
                        };
                        if let (CardKind::Chat(saved), CardKind::Chat(canonical)) =
                            (&*saved, &pane.kind)
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
                    let mut kind = if keep_live {
                        saved
                    } else {
                        Cow::Borrowed(&pane.kind)
                    };
                    if observed_exit.is_some() {
                        preserve_observed_terminal_exit(observed_exit.as_ref(), kind.to_mut());
                    }
                    // A reported title waiting for its save is newer than the
                    // shared one, which it replaces when that save publishes it.
                    let reported = self.reported_title_save.cards.contains(&pane.id);
                    // Writing a card copies it while a save still holds it,
                    // so only a card the view changes is written.
                    let changed = self.card_state(pane.id).is_some_and(|current| {
                        (!reported && !keep_live && current.title != pane.title)
                            || !keep_live && current.width_fraction != pane.width_fraction
                            || (!resuming && current.kind != *kind)
                    });
                    if changed && let Some(current) = self.card_state_mut(pane.id) {
                        if !reported && !keep_live {
                            current.title = pane.title.clone();
                        }
                        if !keep_live {
                            current.width_fraction = pane.width_fraction;
                        }
                        if !resuming {
                            current.kind = kind.into_owned();
                        }
                    }
                    // Publishing a locally opened chat can echo its pane
                    // before the provider handshake creates a runtime. Its
                    // exact in-flight attempt already owns that attachment;
                    // replacing it would discard the first start's reply.
                    // A different canonical descriptor still needs attachment.
                    let starting = matches!(&pane.kind, CardKind::Chat(session) if self.chat_start_matches(pane.id, session));
                    if resuming || self.holds_reopen(pane.id) {
                        // Its start attaches the card, or the close it waits
                        // behind does once it has stopped its conversation.
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
            if self.durable.card_scopes.get(&pane.id) != pane.scope.as_ref() {
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
        }
        // A quarantined pane stays where and as this client shows it; without
        // a local card here, there is nothing this client may show for it.
        let local_pane = |id: CardId| {
            let card = self.card_state(id)?;
            let mut local = Pane::from_card(card, self.durable.card_scopes.get(&id).cloned())?;
            local.user_title = self.chrome_ui.user_titles.get(&id).cloned();
            Some(local)
        };
        let mut strip = view
            .panes
            .iter()
            .filter_map(|pane| {
                if !self.shared_views.quarantined.contains(&pane.id) {
                    Some(Cow::Borrowed(pane))
                } else if local_members.contains(&pane.id) {
                    local_pane(pane.id).map(Cow::Owned)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        let shared = strip.iter().map(|pane| pane.id).collect::<HashSet<_>>();
        let retained = current
            .into_iter()
            .filter(|id| {
                (self.shared_views.quarantined.contains(id) && !shared.contains(id))
                    || self.shared_views.retained.contains(id)
                    || self.reopen_pending(*id)
                    || self.awaits_first_publication(*id, view)
                    || (view.panes.is_empty() && self.chat_is_unused_default(*id))
                    || self
                        .card_state(*id)
                        .is_some_and(|card| card.kind.is_utility())
            })
            .collect::<Vec<_>>();
        let mut visible = strip.iter().map(|pane| pane.id).collect::<Vec<_>>();
        let mut held = Vec::new();
        for id in retained {
            // A fenced card the view does not place keeps its local place:
            // after the nearest card it followed here. So does a pane still
            // reopening, which is not shared until it starts, and a chat yet
            // to be published, rather than jumping to the end of the strip
            // meanwhile.
            if shared.contains(&id)
                || !(self.shared_views.quarantined.contains(&id)
                    || self.reopen_pending(id)
                    || self.awaits_first_publication(id, view))
            {
                visible.push(id);
                continue;
            }
            let before = local_order
                .iter()
                .position(|prior| *prior == id)
                .unwrap_or(local_order.len());
            let place = local_order[..before]
                .iter()
                .rev()
                .find_map(|prior| visible.iter().position(|placed| placed == prior))
                .map_or(0, |index| index + 1);
            visible.insert(place, id);
            held.extend(local_pane(id));
        }
        // The strip shows held cards too, in their visible order.
        for pane in held {
            let place = visible
                .iter()
                .take(visible.iter().position(|id| *id == pane.id).unwrap_or(0))
                .filter(|id| strip.iter().any(|placed| placed.id == **id))
                .count();
            strip.insert(place, Cow::Owned(pane));
        }
        if let Some(index) = index {
            let channel = &self.durable.channels[index];
            let focused = channel
                .focused
                .filter(|id| visible.contains(id))
                .or_else(|| visible.first().copied());
            if channel.cards != visible || channel.focused != focused {
                let channel = &mut Arc::make_mut(&mut self.durable).channels[index];
                channel.cards = visible.clone();
                channel.focused = focused;
            }
        }
        self.normalize_projected_card_order(&visible);
        if active {
            // The strip's one edit also takes out every pane detached so far.
            let detached = std::mem::take(&mut self.shared_views.strip_detached);
            let strip = strip.into_iter().map(Cow::into_owned).collect::<Vec<_>>();
            self.project_shared_strip(&strip, &visible, &detached);
            if !detached.is_empty() {
                self.settle_focus();
            }
        }
        if !self.shared_views.applying {
            self.persist();
        }
        // Pane names live in the window's settings, not its workspace save.
        // Unsaved, the next start reads the old names back, which no longer
        // match the registry's, and migrates the view again (#3032).
        if user_titles_changed {
            tasks.push(self.save_settings_task(false));
        }
        Task::batch(tasks)
    }

    /// Normalize the selected subsequence without moving unrelated history.
    /// Local channel order alone is not proof of durable card order.
    fn normalize_projected_card_order(&mut self, visible: &[CardId]) {
        let mut previous = None;
        let ordered = visible.iter().all(|id| {
            let Some(index) = self.card_index(*id) else {
                return true;
            };
            let follows = previous.is_none_or(|previous| previous < index);
            previous = Some(index);
            follows
        });
        if ordered {
            return;
        }
        #[cfg(test)]
        {
            self.shared_views.order_rewrites += 1;
        }
        Arc::make_mut(&mut self.durable)
            .cards
            .arrange_within(visible);
    }

    pub(super) fn presentation_chrome_cards(&self, scope: &Scope) -> Vec<CardId> {
        self.local_view_cards(scope)
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
        #[cfg(test)]
        {
            self.shared_views.pane_attachment_count += 1;
        }
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
                return self.attach_shared_chat(pane.id, session.clone());
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
        self.shared_views.closed.contains(&id)
            || view.retired.contains(&id)
            || self
                .workspace_model
                .views
                .iter()
                .any(|view| view.retired.contains(&id))
    }

    /// Whether this window closed `id` and keeps it closed whatever a view
    /// lists: storage holds its tombstone, and an identity is never reopened.
    /// A fenced pane has a close of its own, and so does a chat whose
    /// started conversation is stopping, or that the daemon refused to stop.
    ///
    /// Reads `card_indices`, so a caller first brings them up to date.
    fn closed_here(&self, id: CardId) -> bool {
        self.card_indices
            .get(&id)
            .and_then(|index| self.durable.cards.get(*index))
            .is_some_and(|card| card.id == id && matches!(card.existence, Existence::Closed { .. }))
            && !self.shared_views.quarantined.contains(&id)
            && !self.chat_close_returns_through_stop(id)
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
    /// The pane leaves the strip later, in one edit with everything else the
    /// projection changes there: see `SharedViews::strip_detached`.
    fn detach_shared_pane(&mut self, id: CardId) {
        self.shared_views.defaults.remove(&id);
        Arc::make_mut(&mut self.durable)
            .pending_terminal_creations
            .remove(&id);
        self.shared_views.default_requests.remove(&id);
        self.shared_chat_detach(id);
        self.retire_lsp_card(id);
        #[cfg(feature = "integrated-browser")]
        if let Some(CardRuntime::Browser(browser)) = self.cards.remove(&id) {
            self.begin_browser_teardown(browser.handle);
        }
        #[cfg(not(feature = "integrated-browser"))]
        self.cards.remove(&id);
        self.lifecycle.remove(id);
        if let Some(card) = self.card_state_mut(id) {
            card.existence = super::project_ui::removed_existence("pane closed in another client");
        }
        self.sidebar_state.forget_card_attention(id);
        self.shared_views.strip_detached.insert(id);
    }

    /// Takes the panes [`Self::detach_shared_pane`] detached, and no
    /// projection has yet, out of the strip in one edit.
    fn remove_detached_from_strip(&mut self) {
        let detached = std::mem::take(&mut self.shared_views.strip_detached);
        if detached.is_empty() {
            return;
        }
        let _ = self.workspace.remove_cards(&detached);
        // The strip handed a removed pane's focus to a neighbour, with no
        // viewport update to tell the saved state.
        self.settle_focus();
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
        let viewport = self.workspace.capture_viewport_anchor();
        self.project_shared_strip(&panes, &visible, &HashSet::new());
        self.finish_shared_viewport_projection(viewport)
    }

    /// Commit a passive strip edit as one layout that keeps the pane being
    /// read, and any navigation already in flight, where the reader left them.
    fn finish_shared_viewport_projection(
        &mut self,
        anchor: crate::workspace::ViewportAnchor,
    ) -> Task<Message> {
        self.remove_detached_from_strip();
        let update = self.workspace.restore_viewport_anchor(anchor);
        // Most snapshots leave the strip and its focus alone and need no
        // viewport pass; the sweep still settles descriptors they replaced.
        let viewport = if update == ViewportUpdate::unchanged()
            && self.workspace.focused() == self.durable.focused
        {
            Task::none()
        } else {
            self.apply_viewport(update, true, DurableChange::None)
        };
        Task::batch([viewport, self.lifecycle_sweep()])
    }

    /// Mirrors `panes` into the strip without moving the view: the caller
    /// captured a [`crate::workspace::ViewportAnchor`] first and restores it
    /// with [`Self::finish_shared_viewport_projection`] once the batch ends.
    ///
    /// The strip keeps the cards in `visible` and closes the rest, along with
    /// any in `detached`, whose surfaces are already gone. The panes come
    /// first, in their order, followed by the other cards it keeps, in theirs.
    /// The whole edit is one [`crate::workspace::Workspace::arrange`].
    fn project_shared_strip(
        &mut self,
        panes: &[Pane],
        visible: &[CardId],
        detached: &HashSet<CardId>,
    ) {
        let mut visible = visible.iter().copied().collect::<HashSet<_>>();
        // A pane the user is closing here stays until that close finishes,
        // which a transfer elsewhere must not turn into a failed one.
        visible.extend(
            self.workspace
                .pending_close()
                .map(|pending| pending.unmounting),
        );
        let removed = self
            .workspace
            .order()
            .filter(|id| !visible.contains(id) && !detached.contains(id))
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
        }
        let mut placed = HashSet::with_capacity(panes.len());
        let mut cards = Vec::with_capacity(panes.len());
        for pane in panes {
            if detached.contains(&pane.id) || !placed.insert(pane.id) {
                continue;
            }
            // A held reported title is newer than the shared one.
            let held = self.reported_title_save.cards.contains(&pane.id);
            let card = match self.workspace.card(pane.id) {
                Some(current) => {
                    // A name or width the strip refuses leaves the one it has.
                    let accessible_name = (pane.user_title.is_some() || !held)
                        .then(|| {
                            crate::workspace::normalize_card_name(
                                pane.user_title.as_deref().unwrap_or_else(|| {
                                    browser_host::presentation_title(&pane.kind, &pane.title)
                                }),
                            )
                        })
                        .flatten()
                        .unwrap_or_else(|| current.accessible_name.clone());
                    Some(WorkspaceCard {
                        id: pane.id,
                        accessible_name,
                        width: CardWidth::fraction(pane.width_fraction).unwrap_or(current.width),
                    })
                }
                None => {
                    let held = held
                        .then(|| self.card_state(pane.id).map(|card| card.title.as_str()))
                        .flatten();
                    WorkspaceCard::new(
                        pane.id,
                        pane.user_title.as_deref().or(held).unwrap_or_else(|| {
                            browser_host::presentation_title(&pane.kind, &pane.title)
                        }),
                        CardWidth::Fraction(pane.width_fraction),
                    )
                    .ok()
                }
            };
            cards.extend(card);
        }
        cards.extend(
            self.workspace
                .cards()
                .iter()
                .filter(|card| {
                    visible.contains(&card.id)
                        && !detached.contains(&card.id)
                        && !placed.contains(&card.id)
                })
                .cloned(),
        );
        let _ = self.workspace.arrange(cards);
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
mod tests;
