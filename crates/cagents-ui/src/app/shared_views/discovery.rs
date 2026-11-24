//! Scope discovery kept between complete saves.
//!
//! A complete save publishes the scopes this window shows, but finding them
//! meant rediscovering every scope it ever showed: every listed worktree
//! view, every known root and every card's worktree, in a workspace that
//! accumulates thousands of each. A pass now keeps three results between
//! saves:
//!
//! - the scope topology, from the sidebar's projects and the known roots;
//! - each card's worktree, from the card's directory, owner, filing and
//!   channel;
//! - the discovered worktree scopes, with what makes each one discovered,
//!   and which of them are not yet seeded or settled.
//!
//! The topology is derived again when the projects or the known roots
//! change, and everything after it with it. Otherwise a pass follows what
//! changed: a card's worktree is resolved again when its own inputs differ, a
//! worktree gaining its first card or losing its last adds or drops its
//! scope, the listed views are compared with those last seen, and the seeded
//! and settled key sets journal the scopes that joined or left them. Seeding
//! one worktree therefore touches one scope. Inputs are compared by value or
//! by a revision their owner takes on every change, never by counting on each
//! writer to report one.
//!
//! Channels are few and every one is offered, so they are not kept. Nor is
//! the active workspace, which navigation changes on most saves: its scope
//! is offered on its own. Everything the publishable filter reads is read
//! afresh, so a pass keeps the scopes, members and panes a complete
//! rediscovery would. Their order is not always the same. A rediscovery
//! lists the channels, then the listed views' scopes in model order, then
//! the known roots' in hash order, the active workspace's and the cards'.
//! Here channels keep their order, but worktree scopes come in the order
//! they were first discovered since the last complete discovery, so one
//! found since then comes last. Publication does not depend on that order:
//! each scope's edits are its own, and transfers go out in map order. The
//! exception is a path two scopes name, which only a listed view under
//! another repository than the topology resolves for it can add. Both
//! scopes list the path's cards, and the first published takes a default
//! card's `InsertIfEmpty` request: the listed view's in a rediscovery, but
//! here the path's own scope's if that was discovered before the view was
//! listed.

use super::settlement::KeyChanges;
use super::*;
use crate::revision::next_revision;
use crate::sidebar::{Revisioned, SidebarProject};
use std::sync::atomic::Ordering::Relaxed;

#[derive(Default)]
pub(in crate::app) struct ViewDiscovery {
    /// Shared with the structural pass, which reads it while it mutates the
    /// window and before its save refreshes discovery.
    topology: Option<Arc<Topology>>,
    cards: CardWorktrees,
    worktrees: Option<Worktrees>,
    /// Passes that followed changes rather than discovering every worktree
    /// scope again, so tests can tell the kept scopes were exercised.
    #[cfg(test)]
    pub(super) reused: usize,
    /// Leaves out the comparison with a complete rediscovery, for timing
    /// workloads that measure a pass.
    #[cfg(test)]
    pub(super) unchecked: bool,
}

/// What `view_scope_topology` builds, kept between saves with what it was
/// built from.
pub(super) struct Topology {
    revision: u64,
    roots_generation: u64,
    /// Advanced in place when the projects take a revision without changing
    /// what this was built from, so a pass holding it keeps it.
    projects_revision: std::sync::atomic::AtomicU64,
    /// Each project's repository and worktree paths, in sidebar order, for
    /// when the projects took a revision without changing them.
    projects: Vec<(PathBuf, Vec<PathBuf>)>,
    repositories: HashMap<PathBuf, PathBuf>,
    project_repositories: HashSet<PathBuf>,
    roots: HashSet<PathBuf>,
}

impl Topology {
    /// The topology a structural pass resolves scopes and card worktrees
    /// with, borrowed rather than built again.
    pub(super) fn scope_topology(&self) -> ViewScopeTopology<'_> {
        ViewScopeTopology {
            roots: worktree_ui::WorktreeRoots::indexed(&self.roots),
            authorities: ViewScopeAuthorities::Kept {
                repositories: &self.repositories,
                projects: &self.project_repositories,
            },
        }
    }

    fn lists(&self, projects: &[SidebarProject]) -> bool {
        self.projects.len() == projects.len()
            && self
                .projects
                .iter()
                .zip(projects)
                .all(|((repository, worktrees), project)| {
                    *repository == project.repository
                        && worktrees.len() == project.worktrees.len()
                        && worktrees
                            .iter()
                            .zip(&project.worktrees)
                            .all(|(path, worktree)| *path == worktree.path)
                })
    }

    /// As [`ViewScopeAuthorities::repository_for`].
    fn repository_for(&self, path: &Path) -> Option<&Path> {
        self.repositories.get(path).map(PathBuf::as_path)
    }

    /// As [`ViewScopeAuthorities::contains_repository`].
    fn contains_repository(&self, repository: &Path) -> bool {
        self.project_repositories.contains(repository)
    }

    fn worktree_scope(&self, path: &Path) -> Option<Scope> {
        let repository = self.repository_for(path)?.to_owned();
        Some(Scope::Worktree {
            repository,
            path: path.to_owned(),
        })
    }
}

/// Each card's worktree, as `unchanneled_card_worktree_indexed` resolves it
/// for a card no channel lists, with what it was resolved from.
#[derive(Default)]
struct CardWorktrees {
    cards: HashMap<CardId, CardWorktree>,
    /// How many cards resolve to each worktree.
    paths: HashMap<PathBuf, usize>,
    /// The worktrees that gained their first card or lost their last since
    /// the kept scopes last followed them.
    changed: Vec<PathBuf>,
    pass: u64,
}

struct CardWorktree {
    pass: u64,
    topology: u64,
    channeled: bool,
    owner: Option<PathBuf>,
    filed: Option<PathBuf>,
    directory: Option<PathBuf>,
    worktree: Option<PathBuf>,
}

impl CardWorktree {
    fn resolved_from(
        &self,
        topology: u64,
        channeled: bool,
        owner: Option<&Path>,
        filed: Option<&Path>,
        directory: Option<&Path>,
    ) -> bool {
        self.topology == topology
            && self.channeled == channeled
            && self.owner.as_deref() == owner
            && self.filed.as_deref() == filed
            && self.directory.as_deref() == directory
    }
}

impl CardWorktrees {
    fn count(&mut self, worktree: Option<&Path>, added: bool) {
        let Some(worktree) = worktree else {
            return;
        };
        if added {
            let count = self.paths.entry(worktree.to_owned()).or_default();
            *count += 1;
            if *count == 1 {
                self.changed.push(worktree.to_owned());
            }
        } else if let Some(count) = self.paths.get_mut(worktree) {
            *count -= 1;
            if *count == 0 {
                self.paths.remove(worktree);
                self.changed.push(worktree.to_owned());
            }
        }
    }
}

/// What makes a worktree scope discovered. It stays while any holds.
#[derive(Default)]
struct Sources {
    /// When the scope was discovered, which orders the scopes a pass offers.
    order: u64,
    /// Listed views naming the scope.
    views: usize,
    /// A known root at its path.
    root: bool,
    /// A card outside every channel at its path.
    cards: bool,
}

impl Sources {
    fn any(&self) -> bool {
        self.views > 0 || self.root || self.cards
    }
}

/// The seeded and settled scopes' key sets, as taken for one pass.
pub(super) struct Settlement {
    pub(super) initialized: KeyChanges,
    pub(super) baseline: KeyChanges,
}

/// The discovered worktree scopes, kept with what they were derived from.
struct Worktrees {
    topology: u64,
    initialized: u64,
    baseline: u64,
    /// The listed views' scopes, as last followed.
    views: Vec<Scope>,
    sources: HashMap<Scope, Sources>,
    /// The scopes at each path. A listed view can name a path under another
    /// repository than the one its topology resolves.
    at_path: HashMap<PathBuf, Vec<Scope>>,
    /// The scopes not yet seeded, or seeded with no settled baseline, which
    /// the publishable filter keeps whatever they hold.
    unsettled: HashSet<Scope>,
    discovered: u64,
}

impl Worktrees {
    fn change(&mut self, app: &Cagents, scope: Scope, change: impl FnOnce(&mut Sources)) {
        if let Some(sources) = self.sources.get_mut(&scope) {
            change(sources);
            if !sources.any() {
                self.sources.remove(&scope);
                self.unsettled.remove(&scope);
                let Scope::Worktree { path, .. } = &scope else {
                    unreachable!("only worktree scopes are kept");
                };
                if let Some(scopes) = self.at_path.get_mut(path) {
                    scopes.retain(|kept| *kept != scope);
                    if scopes.is_empty() {
                        self.at_path.remove(path);
                    }
                }
            }
            return;
        }
        self.discovered += 1;
        let mut sources = Sources {
            order: self.discovered,
            ..Sources::default()
        };
        change(&mut sources);
        if !sources.any() {
            return;
        }
        let Scope::Worktree { path, .. } = &scope else {
            unreachable!("only worktree scopes are kept");
        };
        self.at_path
            .entry(path.clone())
            .or_default()
            .push(scope.clone());
        self.sources.insert(scope.clone(), sources);
        self.settle(app, scope);
    }

    /// Reads again whether a kept `scope` is seeded and settled.
    fn settle(&mut self, app: &Cagents, scope: Scope) {
        if !self.sources.contains_key(&scope) {
            return;
        }
        let views = &app.shared_views;
        if views.initialized.contains(&scope) && views.baseline.contains_key(&scope) {
            self.unsettled.remove(&scope);
        } else {
            self.unsettled.insert(scope);
        }
    }

    fn view_scope<'v>(topology: &Topology, scope: &'v Scope) -> Option<&'v Scope> {
        match scope {
            Scope::Worktree { repository, .. } if topology.contains_repository(repository) => {
                Some(scope)
            }
            _ => None,
        }
    }

    /// Follows the listed views from those last seen to `views`, comparing
    /// past their common start and end.
    fn follow_views(&mut self, app: &Cagents, topology: &Topology, views: &[View]) {
        let kept = &self.views;
        if kept.len() == views.len()
            && kept
                .iter()
                .zip(views)
                .all(|(kept, view)| *kept == view.scope)
        {
            return;
        }
        let start = kept
            .iter()
            .zip(views)
            .take_while(|(kept, view)| **kept == view.scope)
            .count();
        let end = kept[start..]
            .iter()
            .rev()
            .zip(views[start..].iter().rev())
            .take_while(|(kept, view)| **kept == view.scope)
            .count();
        let left = self
            .views
            .splice(
                start..self.views.len() - end,
                views[start..views.len() - end]
                    .iter()
                    .map(|view| view.scope.clone()),
            )
            .collect::<Vec<_>>();
        for scope in left {
            if let Some(scope) = Self::view_scope(topology, &scope) {
                self.change(app, scope.clone(), |sources| {
                    debug_assert!(sources.views > 0, "a view left that never came");
                    sources.views = sources.views.saturating_sub(1)
                });
            }
        }
        for view in &views[start..views.len() - end] {
            if let Some(scope) = Self::view_scope(topology, &view.scope) {
                self.change(app, scope.clone(), |sources| sources.views += 1);
            }
        }
    }
}

impl Cagents {
    /// [`Self::local_views`], keeping in `discovery` what the next pass can
    /// follow rather than discover again. `settlement` holds the seeded and
    /// settled scopes' journals, taken for this pass.
    pub(super) fn discovered_local_views(
        &self,
        discovery: &mut ViewDiscovery,
        settlement: Settlement,
    ) -> LocalViews {
        let mut phase = WorkspaceProjectionTiming::new(true);
        let revision = self.refresh_view_topology(discovery);
        let channel_cards = self
            .durable
            .channels
            .iter()
            .flat_map(|channel| channel.cards.iter().copied())
            .collect::<HashSet<_>>();
        let topology = discovery.topology.as_ref().expect("refreshed above");
        let (shown, channeled) =
            self.refresh_card_worktrees(&mut discovery.cards, topology, &channel_cards);
        let followed = match discovery.worktrees.as_mut() {
            Some(worktrees) => {
                self.follow_worktrees(worktrees, topology, &mut discovery.cards, &settlement)
            }
            None => Err("none kept"),
        };
        tracing::debug!(target: "cagents::workspace_pipeline", stage = "kept_discovery",
            rediscovered = followed.err(),
            initialized_changes = settlement.initialized.changed.as_ref().map(Vec::len),
            baseline_changes = settlement.baseline.changed.as_ref().map(Vec::len),
            "Followed or discovered again the kept worktree scopes");
        if followed.is_ok() {
            #[cfg(test)]
            {
                discovery.reused += 1;
            }
        } else {
            discovery.cards.changed.clear();
            discovery.worktrees =
                Some(self.discover_worktrees(topology, &discovery.cards, revision, &settlement));
        }
        let ViewDiscovery {
            topology: Some(topology),
            cards,
            worktrees: Some(worktrees),
            ..
        } = &*discovery
        else {
            unreachable!("derived above");
        };
        // Only the cards a worktree shows: the rest are no pane of one.
        let worktree_cards = shown
            .into_iter()
            .filter_map(|card| {
                let worktree = cards.cards.get(&card.id)?.worktree.as_deref()?;
                Some((card, worktree))
            })
            .collect::<Vec<_>>();
        // Every worktree scope the publishable filter can keep: those not
        // settled, those with published panes, those holding a card here and
        // the active workspace's, in the order they were discovered.
        let kept = |scope: &Scope| worktrees.sources.get_key_value(scope);
        let mut offered = worktrees
            .unsettled
            .iter()
            .filter_map(kept)
            .collect::<Vec<_>>();
        offered.extend(
            self.shared_views
                .baseline
                .iter()
                .filter(|(_, panes)| !panes.is_empty())
                .filter_map(|(scope, _)| kept(scope)),
        );
        let mut held = HashSet::new();
        for (card, path) in &worktree_cards {
            if shows_in_worktree(card)
                && held.insert(*path)
                && let Some(scopes) = worktrees.at_path.get(*path)
            {
                offered.extend(scopes.iter().filter_map(kept));
            }
        }
        let active = self
            .active_workspace
            .as_deref()
            .and_then(|path| topology.worktree_scope(path));
        let active = match active.as_ref().map(kept) {
            Some(Some(scope)) => {
                offered.push(scope);
                None
            }
            _ => active,
        };
        offered.sort_unstable_by_key(|(_, sources)| sources.order);
        offered.dedup_by_key(|(_, sources)| sources.order);
        let offered = offered
            .into_iter()
            .map(|(scope, _)| Cow::Borrowed(scope))
            .chain(active.map(Cow::Owned));
        let mut channels = HashSet::new();
        let scopes = self
            .durable
            .channels
            .iter()
            .filter(|channel| channels.insert(channel.id))
            .map(|channel| Cow::Owned(Scope::Channel(channel.id)))
            .chain(offered)
            .collect();
        phase.mark("local_discover");
        let projected = self.project_local_views(
            None,
            ScopeDiscovery {
                scopes,
                worktree_cards,
                channeled,
            },
            true,
            phase,
        );
        #[cfg(test)]
        if !discovery.unchecked {
            self.assert_rediscovery_agrees(topology, worktrees, &projected);
        }
        projected
    }

    /// The topology revision, rebuilding the topology if the projects or the
    /// known roots differ from those it was built from.
    fn refresh_view_topology(&self, discovery: &mut ViewDiscovery) -> u64 {
        let roots_generation = self.known_worktree_roots.generation();
        let projects = &self.sidebar_model.projects;
        let projects_revision = Revisioned::revision(projects);
        if let Some(topology) = &discovery.topology
            && topology.roots_generation == roots_generation
            && (topology.projects_revision.load(Relaxed) == projects_revision
                || topology.lists(projects))
        {
            topology.projects_revision.store(projects_revision, Relaxed);
            return topology.revision;
        }
        #[cfg(test)]
        self.record_topology_build();
        // Logged where the decision is made: the structural pass and the save
        // after it share this topology, so only the first of them rebuilds it.
        tracing::debug!(target: "cagents::workspace_pipeline", stage = "view_topology",
            known_roots_changed = discovery
                .topology
                .as_ref()
                .is_none_or(|topology| topology.roots_generation != roots_generation),
            "Rebuilt the worktree topology");
        let topology = Topology {
            revision: next_revision(),
            roots_generation,
            projects_revision: projects_revision.into(),
            projects: projects
                .iter()
                .map(|project| {
                    (
                        project.repository.clone(),
                        project
                            .worktrees
                            .iter()
                            .map(|worktree| worktree.path.clone())
                            .collect(),
                    )
                })
                .collect(),
            repositories: self.owned_view_repositories(),
            project_repositories: projects
                .iter()
                .map(|project| project.repository.clone())
                .collect(),
            roots: self.worktree_root_paths().into_iter().collect(),
        };
        let revision = topology.revision;
        discovery.topology = Some(Arc::new(topology));
        revision
    }

    /// The kept topology, built again first if the projects or the known
    /// roots changed, for a structural pass to share with the save after it.
    pub(super) fn kept_view_topology(&mut self) -> Arc<Topology> {
        let mut discovery = std::mem::take(&mut self.shared_views.discovery);
        self.refresh_view_topology(&mut discovery);
        let topology = Arc::clone(discovery.topology.as_ref().expect("refreshed above"));
        self.shared_views.discovery = discovery;
        topology
    }

    /// Resolves again each card whose directory, owner, filing or channel
    /// changed, or that is new, and forgets the cards that are gone. Returns
    /// the cards a worktree shows that resolve to one, and every card a
    /// channel lists, by identity.
    fn refresh_card_worktrees<'a>(
        &'a self,
        cards: &mut CardWorktrees,
        kept: &Topology,
        channel_cards: &HashSet<CardId>,
    ) -> (Vec<&'a CardState>, HashMap<CardId, &'a CardState>) {
        let topology = kept.revision;
        cards.pass = cards.pass.wrapping_add(1);
        let pass = cards.pass;
        let mut roots = None;
        let mut shown = Vec::new();
        let mut channeled = HashMap::with_capacity(channel_cards.len());
        // Distinct cards seen this pass: when that is every one kept, none
        // is gone.
        let mut seen = 0;
        for card in self.durable.cards.iter() {
            let listed = channel_cards.contains(&card.id);
            if listed {
                channeled.insert(card.id, card);
            }
            let owner = self.worktree_owners.get(&card.id).map(PathBuf::as_path);
            let filed = self.durable.card_scopes.get(&card.id).map(PathBuf::as_path);
            let directory = worktree_ui::card_directory(card);
            if let Some(entry) = cards.cards.get_mut(&card.id)
                && entry.resolved_from(topology, listed, owner, filed, directory)
            {
                if entry.pass != pass {
                    entry.pass = pass;
                    seen += 1;
                }
                if entry.worktree.is_some() && shows_in_worktree(card) {
                    shown.push(card);
                }
                continue;
            }
            let roots =
                roots.get_or_insert_with(|| worktree_ui::WorktreeRoots::indexed(&kept.roots));
            let worktree = (!listed)
                .then(|| self.unchanneled_card_worktree_indexed(card, roots))
                .flatten()
                .map(Path::to_owned);
            cards.count(worktree.as_deref(), true);
            if worktree.is_some() && shows_in_worktree(card) {
                shown.push(card);
            }
            let previous = cards.cards.insert(
                card.id,
                CardWorktree {
                    pass,
                    topology,
                    channeled: listed,
                    owner: owner.map(Path::to_owned),
                    filed: filed.map(Path::to_owned),
                    directory: directory.map(Path::to_owned),
                    worktree,
                },
            );
            if previous
                .as_ref()
                .is_none_or(|previous| previous.pass != pass)
            {
                seen += 1;
            }
            if let Some(previous) = previous {
                cards.count(previous.worktree.as_deref(), false);
            }
        }
        if seen != cards.cards.len() {
            let gone = cards
                .cards
                .iter()
                .filter(|(_, entry)| entry.pass != pass)
                .map(|(id, _)| *id)
                .collect::<Vec<_>>();
            for id in gone {
                if let Some(entry) = cards.cards.remove(&id) {
                    cards.count(entry.worktree.as_deref(), false);
                }
            }
        }
        (shown, channeled)
    }

    /// Brings the kept worktree scopes up to date with what changed since
    /// they were last followed, or says why they must be discovered again:
    /// the topology or a key set was replaced, or a journal lost track.
    fn follow_worktrees(
        &self,
        worktrees: &mut Worktrees,
        topology: &Topology,
        cards: &mut CardWorktrees,
        settlement: &Settlement,
    ) -> Result<(), &'static str> {
        let (Some(initialized), Some(baseline)) = (
            settlement.initialized.changed.as_ref(),
            settlement.baseline.changed.as_ref(),
        ) else {
            return Err("too many seeded or settled scopes changed");
        };
        if worktrees.topology != topology.revision {
            return Err("topology");
        }
        if worktrees.initialized != settlement.initialized.identity
            || worktrees.baseline != settlement.baseline.identity
        {
            return Err("seeded or settled scopes replaced");
        }
        worktrees.follow_views(self, topology, &self.workspace_model.views);
        for path in std::mem::take(&mut cards.changed) {
            if let Some(scope) = topology.worktree_scope(&path) {
                let held = cards.paths.contains_key(&path);
                worktrees.change(self, scope, |sources| sources.cards = held);
            }
        }
        for scope in initialized.iter().chain(baseline) {
            worktrees.settle(self, scope.clone());
        }
        Ok(())
    }

    /// Every worktree scope, discovered as `discover_view_scopes_with` does,
    /// but for the active workspace.
    fn discover_worktrees(
        &self,
        topology: &Topology,
        cards: &CardWorktrees,
        revision: u64,
        settlement: &Settlement,
    ) -> Worktrees {
        #[cfg(test)]
        self.shared_views
            .scope_discovery_count
            .set(self.shared_views.scope_discovery_count.get() + 1);
        let mut worktrees = Worktrees {
            topology: revision,
            initialized: settlement.initialized.identity,
            baseline: settlement.baseline.identity,
            views: Vec::new(),
            sources: HashMap::new(),
            at_path: HashMap::new(),
            unsettled: HashSet::new(),
            discovered: 0,
        };
        worktrees.follow_views(self, topology, &self.workspace_model.views);
        for root in self.known_worktree_roots.iter() {
            if let Some(scope) = topology.worktree_scope(root) {
                worktrees.change(self, scope, |sources| sources.root = true);
            }
        }
        for path in cards.paths.keys() {
            if let Some(scope) = topology.worktree_scope(path) {
                worktrees.change(self, scope, |sources| sources.cards = true);
            }
        }
        worktrees
    }

    /// Every pass in tests checks itself against a complete rediscovery,
    /// without counting that one as the window's own.
    ///
    /// The kept scopes, with the active workspace's, must be every worktree
    /// scope rediscovery finds, each filed once under its own path and
    /// counting what discovers it now, and the publishable filter must then
    /// keep the same scopes, members and panes.
    #[cfg(test)]
    fn assert_rediscovery_agrees(
        &self,
        topology: &Topology,
        worktrees: &Worktrees,
        projected: &LocalViews,
    ) {
        let counters = &self.shared_views;
        let saved = (
            counters.local_projection_count.get(),
            counters.topology_build_count.get(),
            counters.scope_discovery_count.get(),
        );
        let rediscovered = self.discover_view_scopes();
        let active = self
            .active_workspace
            .as_deref()
            .and_then(|path| topology.worktree_scope(path))
            .filter(|scope| !worktrees.sources.contains_key(scope));
        assert_eq!(
            worktrees
                .sources
                .keys()
                .chain(active.as_ref())
                .collect::<HashSet<_>>(),
            rediscovered
                .scopes
                .iter()
                .map(Cow::as_ref)
                .filter(|scope| matches!(scope, Scope::Worktree { .. }))
                .collect::<HashSet<_>>(),
            "kept worktree scopes"
        );
        let mut filed = HashSet::new();
        for (path, scopes) in &worktrees.at_path {
            assert!(!scopes.is_empty(), "no scope left at {path:?}");
            for scope in scopes {
                assert!(
                    matches!(scope, Scope::Worktree { path: own, .. } if own == path),
                    "{scope:?} filed under {path:?}"
                );
                assert!(filed.insert(scope), "{scope:?} filed twice");
            }
        }
        assert_eq!(
            filed,
            worktrees.sources.keys().collect::<HashSet<_>>(),
            "scopes by path"
        );
        // What discovers each scope now: the listed views naming it, a known
        // root at its path, a card outside every channel there.
        let mut expected = HashMap::<Scope, (usize, bool, bool)>::new();
        for view in &self.workspace_model.views {
            if let Some(scope) = Worktrees::view_scope(topology, &view.scope) {
                expected.entry(scope.clone()).or_default().0 += 1;
            }
        }
        for root in self.known_worktree_roots.iter() {
            if let Some(scope) = topology.worktree_scope(root) {
                expected.entry(scope).or_default().1 = true;
            }
        }
        for (_, path) in &rediscovered.worktree_cards {
            if let Some(scope) = topology.worktree_scope(path) {
                expected.entry(scope).or_default().2 = true;
            }
        }
        let sources = worktrees
            .sources
            .iter()
            .map(|(scope, sources)| (scope.clone(), (sources.views, sources.root, sources.cards)))
            .collect::<HashMap<_, _>>();
        assert_eq!(sources, expected, "what discovers each kept scope");
        let unsettled = worktrees
            .sources
            .keys()
            .filter(|scope| {
                !(counters.initialized.contains(*scope) && counters.baseline.contains_key(*scope))
            })
            .cloned()
            .collect::<HashSet<_>>();
        assert_eq!(worktrees.unsettled, unsettled, "unsettled kept scopes");
        let complete = self.local_views();
        counters.local_projection_count.set(saved.0);
        counters.topology_build_count.set(saved.1);
        counters.scope_discovery_count.set(saved.2);
        let channels = |views: &LocalViews| {
            views
                .scopes
                .iter()
                .filter(|scope| matches!(scope, Scope::Channel(_)))
                .cloned()
                .collect::<Vec<_>>()
        };
        let scopes = |views: &LocalViews| views.scopes.iter().cloned().collect::<HashSet<_>>();
        assert_eq!(scopes(projected), scopes(&complete), "kept scopes");
        assert_eq!(channels(projected), channels(&complete), "channel order");
        assert_eq!(projected.cards, complete.cards, "scope members");
        // A scope is settled exactly when it is seeded and its complete panes
        // are its baseline, which then stands for them, with no chat yet to
        // be published among them.
        let settled = complete
            .panes
            .iter()
            .filter(|(scope, panes)| {
                counters.initialized.contains(*scope)
                    && counters.baseline.get(*scope) == Some(panes)
                    && !panes.iter().any(|pane| self.unpublished_chat(pane.id))
            })
            .map(|(scope, _)| scope.clone())
            .collect::<HashSet<_>>();
        assert_eq!(projected.settled, settled, "settled scopes");
        assert!(
            complete.settled.is_empty(),
            "a complete projection settles none"
        );
        let mut panes = projected.panes.clone();
        for scope in &projected.settled {
            assert!(
                !panes.contains_key(scope),
                "{scope:?} is settled and projected"
            );
            panes.insert(scope.clone(), counters.baseline[scope].clone());
        }
        assert_eq!(panes, complete.panes, "scope panes");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::channel_open_workload::{Scale, card, large_workspace, terminal};
    use crate::model::{ChannelId, Existence, ProjectChannel, ProjectKind, Tombstone};
    use crate::workspace_model::views::View;

    const SMALL: Scale = Scale {
        worktrees: 24,
        channels: 8,
        chats: 5,
        loose_terminals: 6,
        closed_cards: 16,
    };

    /// SplitMix64, so a failing sequence replays from its seed.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^ (z >> 31)
        }

        fn below(&mut self, bound: usize) -> usize {
            (self.next() % bound as u64) as usize
        }

        fn pick<'a, T>(&mut self, items: &'a [T]) -> Option<&'a T> {
            (!items.is_empty()).then(|| &items[self.below(items.len())])
        }
    }

    /// A pass over the kept discovery that publishes nothing, as a selected
    /// read between saves would leave it.
    fn project(app: &mut Cagents) {
        let mut discovery = std::mem::take(&mut app.shared_views.discovery);
        let settlement = Settlement {
            initialized: app.shared_views.initialized.take_changes(),
            baseline: app.shared_views.baseline.take_changes(),
        };
        drop(app.discovered_local_views(&mut discovery, settlement));
        app.shared_views.discovery = discovery;
    }

    /// The worktree scopes holding an open card here, where the publishable
    /// filter keeps an empty settled scope.
    fn held_scopes(app: &Cagents) -> Vec<Scope> {
        let views = app.local_views();
        views
            .scopes
            .into_iter()
            .filter(|scope| {
                matches!(scope, Scope::Worktree { .. })
                    && views
                        .cards
                        .get(scope)
                        .is_some_and(|cards| !cards.is_empty())
            })
            .collect()
    }

    /// Paths a step can name: listed worktrees and repositories, a directory
    /// inside a root, and directories no root or project knows.
    fn candidate_paths(app: &Cagents, rng: &mut Rng) -> Vec<PathBuf> {
        let mut paths = app
            .sidebar_model
            .projects
            .iter()
            .flat_map(|project| {
                std::iter::once(project.repository.clone())
                    .chain(project.worktrees.iter().map(|row| row.path.clone()))
            })
            .collect::<Vec<_>>();
        paths.extend([
            PathBuf::from("/w/app/src"),
            PathBuf::from("/elsewhere/checkout"),
            PathBuf::from(format!("/w/app-wt/new-{}", rng.below(6))),
        ]);
        paths
    }

    fn closed() -> Existence {
        Existence::Closed {
            tombstone: Tombstone {
                operation_id: None,
                closed_at_unix_ms: 1,
                reason: None,
            },
        }
    }

    /// One edit of something discovery reads, chosen by `rng`.
    fn edit(app: &mut Cagents, rng: &mut Rng) -> &'static str {
        let paths = candidate_paths(app, rng);
        let path = rng.pick(&paths).unwrap().clone();
        let card_ids = app
            .durable
            .cards
            .iter()
            .map(|card| card.id)
            .collect::<Vec<_>>();
        let card_id = rng.pick(&card_ids).copied();
        match rng.below(27) {
            0 => {
                let Some(id) = card_id else { return "none" };
                let durable = Arc::make_mut(&mut app.durable);
                let card = durable.cards.iter_mut().find(|card| card.id == id).unwrap();
                card.existence = if card.existence.is_open() {
                    closed()
                } else {
                    Existence::Open
                };
                "open or close a card"
            }
            1 => {
                let Some(id) = card_id else { return "none" };
                let durable = Arc::make_mut(&mut app.durable);
                durable.cards.retain(|card| card.id != id);
                for channel in &mut durable.channels {
                    channel.cards.retain(|card| *card != id);
                }
                "remove a card"
            }
            2 => {
                let state = card("shell".into(), terminal(&path), Existence::Open);
                let id = state.id;
                let position = rng.below(app.durable.cards.len() + 1);
                let channel = rng.below(app.durable.channels.len() * 2 + 1);
                let durable = Arc::make_mut(&mut app.durable);
                durable.cards.insert(position, state);
                if let Some(channel) = durable.channels.get_mut(channel) {
                    channel.cards.push(id);
                }
                "insert a card"
            }
            3 => {
                let Some(id) = card_id else { return "none" };
                let channel = rng.below(app.durable.channels.len() * 2 + 1);
                let durable = Arc::make_mut(&mut app.durable);
                for channel in &mut durable.channels {
                    channel.cards.retain(|card| *card != id);
                }
                if let Some(channel) = durable.channels.get_mut(channel) {
                    channel.cards.push(id);
                }
                "move a card between channels and worktrees"
            }
            4 => {
                let Some(id) = card_id else { return "none" };
                let durable = Arc::make_mut(&mut app.durable);
                let card = durable.cards.iter_mut().find(|card| card.id == id).unwrap();
                card.kind = terminal(&path);
                "change a card's directory"
            }
            5 => {
                let Some(id) = card_id else { return "none" };
                let scopes = &mut Arc::make_mut(&mut app.durable).card_scopes;
                if scopes.remove(&id).is_none() {
                    scopes.insert(id, path);
                }
                "file or unfile a card"
            }
            6 => {
                let Some(id) = card_id else { return "none" };
                if app.worktree_owners.remove(&id).is_none() {
                    app.worktree_owners.insert(id, path);
                }
                "own or disown a card's worktree"
            }
            7 => {
                app.known_worktree_roots.insert(path);
                "know a root"
            }
            8 => {
                let project = rng.below(app.sidebar_model.projects.len());
                let rows = &mut app.sidebar_model.projects[project].worktrees;
                if rows.len() > 1 && rng.below(2) == 0 {
                    rows.pop();
                    "remove a worktree row"
                } else {
                    rows.push(crate::sidebar::SidebarWorktree::new(
                        crate::model::WorktreeId::new(),
                        path,
                        None,
                        false,
                        false,
                        None,
                        None,
                        None,
                    ));
                    "add a worktree row"
                }
            }
            9 => {
                let projects = &mut app.sidebar_model.projects;
                let (a, b) = (rng.below(projects.len()), rng.below(projects.len()));
                projects.swap(a, b);
                "reorder projects"
            }
            10 => {
                let project = rng.below(app.sidebar_model.projects.len());
                let repository = &mut app.sidebar_model.projects[project].repository;
                *repository = match repository.file_name() {
                    Some(name) if name == ".git" => repository.parent().unwrap().to_owned(),
                    _ => repository.join(".git"),
                };
                "re-point a project's repository"
            }
            11 => {
                let scopes = if rng.below(2) == 0 {
                    held_scopes(app)
                } else {
                    app.local_view_scopes()
                };
                let Some(scope) = rng.pick(&scopes).cloned() else {
                    return "none";
                };
                let views = &mut app.shared_views;
                match rng.below(4) {
                    0 => {
                        let panes = card_id
                            .and_then(|id| app.durable.cards.iter().find(|card| card.id == id))
                            .and_then(|card| Pane::from_card(card, None))
                            .into_iter()
                            .collect();
                        views.baseline.insert(scope.clone(), panes);
                        views.initialized.insert(scope);
                        "seed a scope"
                    }
                    1 => {
                        views.baseline.insert(scope.clone(), Vec::new());
                        views.initialized.insert(scope);
                        "settle a scope empty"
                    }
                    2 => {
                        views.baseline.remove(&scope);
                        "unsettle a scope"
                    }
                    _ => {
                        views.initialized.remove(&scope);
                        "unseed a scope"
                    }
                }
            }
            12 => {
                let views = &mut app.workspace_model.views;
                if !views.is_empty() && rng.below(2) == 0 {
                    views.remove(rng.below(views.len()));
                    "remove a view"
                } else {
                    let repositories = app
                        .sidebar_model
                        .projects
                        .iter()
                        .map(|project| project.repository.clone())
                        .collect::<Vec<_>>();
                    let repository = rng.pick(&repositories).unwrap().clone();
                    // Half name a held worktree, perhaps under another
                    // repository than the topology resolves for it.
                    let held = held_scopes(app);
                    let path = match rng.pick(&held) {
                        Some(Scope::Worktree { path, .. }) if rng.below(2) == 0 => path.clone(),
                        _ => path,
                    };
                    let views = &mut app.workspace_model.views;
                    let mut view = View::empty(ChannelId::new());
                    view.scope = Scope::Worktree { repository, path };
                    let position = rng.below(views.len() + 1);
                    views.insert(position, view);
                    "add a view"
                }
            }
            13 => {
                app.active_workspace = (rng.below(4) != 0).then_some(path);
                "switch the active workspace"
            }
            14 => {
                let kept = app
                    .known_worktree_roots
                    .iter()
                    .filter(|_| rng.below(3) != 0)
                    .cloned()
                    .collect::<Vec<_>>();
                app.known_worktree_roots = kept.into_iter().collect();
                "replace the known roots"
            }
            15 => {
                let channels = &mut Arc::make_mut(&mut app.durable).channels;
                match rng.below(3) {
                    0 if !channels.is_empty() => {
                        channels.remove(rng.below(channels.len()));
                        "remove a channel"
                    }
                    1 if channels.len() > 1 => {
                        let (a, b) = (rng.below(channels.len()), rng.below(channels.len()));
                        channels.swap(a, b);
                        "reorder channels"
                    }
                    _ => {
                        let template = channels.first().cloned();
                        let Some(template) = template else {
                            return "none";
                        };
                        channels.push(ProjectChannel {
                            id: ChannelId::new(),
                            cards: Vec::new(),
                            focused: None,
                            ..template
                        });
                        "add a channel"
                    }
                }
            }
            16 => {
                let cards = &mut Arc::make_mut(&mut app.durable).cards;
                if cards.is_empty() {
                    return "none";
                }
                let (a, b) = (rng.below(cards.len()), rng.below(cards.len()));
                cards.swap(a, b);
                "reorder cards"
            }
            17 => {
                let views = &mut app.workspace_model.views;
                if views.is_empty() {
                    return "none";
                }
                let (a, b) = (rng.below(views.len()), rng.below(views.len()));
                views.swap(a, b);
                "reorder views"
            }
            18 => {
                let state = card("Settings".into(), CardKind::Settings, Existence::Open);
                let id = state.id;
                let durable = Arc::make_mut(&mut app.durable);
                durable.cards.push(state);
                if rng.below(2) == 0 {
                    durable.card_scopes.insert(id, path);
                } else {
                    app.worktree_owners.insert(id, path);
                }
                "add a utility card"
            }
            19 => {
                let project = rng.below(app.sidebar_model.projects.len());
                let kind = &mut app.sidebar_model.projects[project].kind;
                *kind = match kind {
                    ProjectKind::Git => ProjectKind::Folder,
                    ProjectKind::Folder => ProjectKind::Git,
                };
                // Admits a Folder project's root, as a sidebar pass does.
                app.refresh_sidebar_worktree_panes();
                "turn a project into a folder or back"
            }
            20 => {
                let seeding = app.shared_views.seeding.iter().cloned().collect::<Vec<_>>();
                let Some(scope) = rng.pick(&seeding).cloned() else {
                    return "none";
                };
                app.acknowledge_view_edit(scope, None);
                "acknowledge a seed"
            }
            21 => {
                let views = app.local_views();
                let held = views
                    .cards
                    .iter()
                    .filter_map(|(scope, cards)| Some((scope.clone(), *cards.first()?)))
                    .collect::<Vec<_>>();
                let Some((scope, card)) = rng.pick(&held).cloned() else {
                    return "none";
                };
                app.shared_views.baseline.remove(&scope);
                app.shared_views.initialized.insert(scope.clone());
                app.publish_new_managed_pane(scope, card);
                "publish a new managed pane"
            }
            22 => {
                // A change, then a copy put in the set's place, as tests do:
                // the journal that recorded the change goes with the set.
                let scopes = app.local_view_scopes();
                let Some(scope) = rng.pick(&scopes).cloned() else {
                    return "none";
                };
                let views = &mut app.shared_views;
                if rng.below(2) == 0 {
                    if !views.initialized.remove(&scope) {
                        views.initialized.insert(scope);
                    }
                    views.initialized = views.initialized.clone();
                } else {
                    if views.baseline.remove(&scope).is_none() {
                        views.baseline.insert(scope, Vec::new());
                    }
                    views.baseline = views.baseline.clone();
                }
                "replace the seeded or settled scopes"
            }
            23 => {
                let Some(id) = card_id else { return "none" };
                let durable = Arc::make_mut(&mut app.durable);
                let card = durable.cards.iter_mut().find(|card| card.id == id).unwrap();
                card.kind = CardKind::Settings;
                "turn a card into a utility card"
            }
            24 => {
                let project = rng.pick(&app.sidebar_model.projects).unwrap();
                let view = crate::model::RepositoryView {
                    project: project.id,
                    repository: project.repository.clone(),
                    worktree: path,
                    selected_file: None,
                };
                let state = card(
                    "project".into(),
                    CardKind::Repository(view),
                    Existence::Open,
                );
                let position = rng.below(app.durable.cards.len() + 1);
                Arc::make_mut(&mut app.durable)
                    .cards
                    .insert(position, state);
                "open a repository card"
            }
            25 => {
                // A sidebar pass that lists the same projects again.
                app.sidebar_model.projects = app.sidebar_model.projects.to_vec().into();
                "list the same projects again"
            }
            _ => {
                app.shared_views = State::for_workspace(&app.durable);
                "join a new workspace model"
            }
        }
    }

    /// Across randomized edits of everything discovery reads, each pass over
    /// the kept discovery projects what a complete rediscovery does: every
    /// pass in tests compares the two. Saves between edits also let
    /// publication seed and settle the scopes it acts on.
    #[test]
    fn kept_discovery_agrees_with_rediscovery_across_random_edits() {
        let (mut passes, mut followed) = (0, 0);
        for seed in 0..12 {
            let (mut app, _, _directory) = large_workspace(&SMALL);
            let mut rng = Rng(seed);
            for step in 0..150 {
                let edit = edit(&mut app, &mut rng);
                // A new model replaces the kept discovery with it.
                let before = app.shared_views.discovery.reused;
                let run = || {
                    if rng.below(3) == 0 {
                        project(&mut app);
                    } else {
                        app.publish_view_changes();
                    }
                };
                if let Err(panic) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(run)) {
                    eprintln!("seed {seed}, step {step}: after {edit}");
                    std::panic::resume_unwind(panic);
                }
                passes += 1;
                followed += app.shared_views.discovery.reused - before;
            }
        }
        eprintln!("{followed} of {passes} passes followed the kept scopes");
        // Most edits leave the topology and both key sets in place.
        assert!(followed * 2 > passes, "{followed} of {passes}");
    }

    /// One edit of what a scope's panes are made of, chosen by `rng`.
    fn pane_edit(app: &mut Cagents, rng: &mut Rng) -> &'static str {
        let open = app
            .durable
            .cards
            .iter()
            .filter(|card| card.existence.is_open())
            .map(|card| card.id)
            .collect::<Vec<_>>();
        let Some(&id) = rng.pick(&open) else {
            return "none";
        };
        let toggle = |set: &mut HashSet<CardId>| {
            if !set.remove(&id) {
                set.insert(id);
            }
        };
        match rng.below(15) {
            0 => {
                app.card_state_mut(id).unwrap().title = format!("title {}", rng.below(3));
                "retitle a card"
            }
            1 => {
                let titles = &mut app.chrome_ui.user_titles;
                if titles.remove(&id).is_none() {
                    titles.insert(id, format!("named {}", rng.below(3)));
                }
                "name or unname a pane"
            }
            2 => {
                app.card_state_mut(id).unwrap().width_fraction = [0.3, 0.5, 0.7][rng.below(3)];
                "resize a pane"
            }
            3 => {
                toggle(&mut Arc::make_mut(&mut app.durable).pending_terminal_creations);
                "start or settle a terminal creation"
            }
            4 => {
                toggle(&mut app.shared_views.retained);
                "keep a pane local or share it"
            }
            5 => {
                app.reported_title_save.cards.insert(id);
                app.card_state_mut(id).unwrap().title = format!("reported {}", rng.below(3));
                "report a title before its save"
            }
            6 => {
                toggle(&mut app.shared_views.quarantined);
                "fence or unfence a pane"
            }
            7 => {
                let channels = &mut Arc::make_mut(&mut app.durable).channels;
                let Some(channel) = channels.iter_mut().find(|channel| channel.cards.len() > 1)
                else {
                    return "none";
                };
                let (a, b) = (
                    rng.below(channel.cards.len()),
                    rng.below(channel.cards.len()),
                );
                channel.cards.swap(a, b);
                "reorder a channel's panes"
            }
            8 => {
                app.reported_title_save.cards.clear();
                "save reported titles"
            }
            9 | 10 => {
                // The registry acknowledges what this window published: its
                // view lists the baseline at a later revision.
                let listed = app
                    .workspace_model
                    .views
                    .iter()
                    .enumerate()
                    .filter_map(|(index, view)| {
                        app.shared_views
                            .baseline
                            .get(&view.scope)
                            .map(|baseline| (index, baseline.clone()))
                    })
                    .collect::<Vec<_>>();
                let Some((index, baseline)) = rng.pick(&listed).cloned() else {
                    return "none";
                };
                let view = &mut app.workspace_model.views[index];
                view.panes = baseline;
                view.revision += 1 + rng.below(3) as u64;
                "list a scope's published panes"
            }
            11 => {
                // A terminal listed with its start pending, or settled.
                let listed = app
                    .workspace_model
                    .views
                    .iter()
                    .enumerate()
                    .flat_map(|(view, entry)| {
                        entry
                            .panes
                            .iter()
                            .enumerate()
                            .filter(|(_, pane)| matches!(pane.kind, CardKind::Terminal(_)))
                            .map(move |(pane, _)| (view, pane))
                    })
                    .collect::<Vec<_>>();
                let Some(&(view, pane)) = rng.pick(&listed) else {
                    return "none";
                };
                let pane = &mut app.workspace_model.views[view].panes[pane];
                pane.pending_terminal_start = !pane.pending_terminal_start;
                "list a terminal's start as pending or settled"
            }
            12 => {
                // A browser, which shares no scroll, then scrolled.
                let card = app.card_state_mut(id).unwrap();
                match &mut card.kind {
                    CardKind::Browser(session) => {
                        session.scroll_y = [0., 120., 480.][rng.below(3)];
                        "scroll a browser"
                    }
                    kind => {
                        *kind = CardKind::Browser(crate::model::BrowserSession {
                            id: crate::model::BrowserId::new(),
                            current_url: "https://example.com".parse().unwrap(),
                            history: Vec::new(),
                            history_index: 0,
                            scroll_x: 0.,
                            scroll_y: 40.,
                            fidelity: crate::model::BrowserRestoreFidelity::UrlOnly,
                        });
                        "turn a card into a scrolled browser"
                    }
                }
            }
            13 => {
                let scopes = app
                    .shared_views
                    .baseline
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>();
                let Some(scope) = rng.pick(&scopes).cloned() else {
                    return "none";
                };
                let blocked = &mut app.shared_views.blocked;
                if !blocked.remove(&scope) {
                    blocked.insert(scope);
                }
                "block or unblock a scope"
            }
            _ => {
                let CardKind::Terminal(session) = &app.card_state(id).unwrap().kind else {
                    return "none";
                };
                let session = session.clone();
                let defaults = &mut app.shared_views.defaults;
                if defaults.remove(&id).is_none() {
                    defaults.insert(id, session);
                }
                "start or settle a default terminal"
            }
        }
    }

    /// Model edits as a publication sends them, without their random
    /// operation identities: seeds and each scope's changes in order, and
    /// transfers, which go out in map order, as a sorted list.
    #[derive(Debug, PartialEq)]
    struct Published {
        seeds: HashMap<Scope, ModelEdit>,
        changes: HashMap<Scope, Vec<(u64, Change)>>,
        transfers: Vec<String>,
    }

    fn published(requests: &crate::app::workspace_model_sync::TestModelRequests) -> Published {
        let mut published = Published {
            seeds: HashMap::new(),
            changes: HashMap::new(),
            transfers: Vec::new(),
        };
        while let Ok(edit) = requests.try_recv() {
            match edit {
                ModelEdit::SeedView { ref scope, .. } => {
                    assert!(published.seeds.insert(scope.clone(), edit).is_none());
                }
                ModelEdit::ChangeView {
                    scope,
                    expected_revision,
                    change: change @ Change::Transfer { .. },
                    ..
                } => published
                    .transfers
                    .push(format!("{scope:?} {expected_revision} {change:?}")),
                ModelEdit::ChangeView {
                    scope,
                    expected_revision,
                    change,
                    ..
                } => published
                    .changes
                    .entry(scope)
                    .or_default()
                    .push((expected_revision, change)),
                other => panic!("a publication sent {other:?}"),
            }
        }
        published.transfers.sort();
        published
    }

    /// Whether `change` asks to insert one of `panes` into an empty view.
    fn inserts_one_of(change: &Change, panes: &HashSet<CardId>) -> bool {
        matches!(change, Change::InsertIfEmpty { pane } if panes.contains(&pane.id))
    }

    /// `published` without the `ignored` panes' `InsertIfEmpty` requests.
    fn published_without(mut published: Published, ignored: &HashSet<CardId>) -> Published {
        published.changes.retain(|_, changes| {
            changes.retain(|(_, change)| !inserts_one_of(change, ignored));
            !changes.is_empty()
        });
        published
    }

    /// The panes `published` asks to insert into an empty view.
    fn inserted_if_empty(published: &Published) -> impl Iterator<Item = CardId> + '_ {
        published
            .changes
            .values()
            .flatten()
            .filter_map(|(_, change)| match change {
                Change::InsertIfEmpty { pane } => Some(pane.id),
                _ => None,
            })
    }

    /// What a publication leaves of the state it writes, without operation
    /// identities.
    fn publication_state(views: &State) -> impl PartialEq + std::fmt::Debug {
        publication_state_without(views, &HashSet::new())
    }

    /// [`publication_state`] without the `ignored` panes' `InsertIfEmpty`
    /// requests and baseline entries.
    fn publication_state_without(
        views: &State,
        ignored: &HashSet<CardId>,
    ) -> impl PartialEq + std::fmt::Debug + use<> {
        let mut pending = views
            .pending
            .iter()
            .filter(|(_, _, change)| !inserts_one_of(change, ignored))
            .map(|(_, scope, change)| format!("{scope:?} {change:?}"))
            .collect::<Vec<_>>();
        pending.sort();
        let mut transfers = views
            .requested_transfers
            .values()
            .copied()
            .collect::<Vec<_>>();
        transfers.sort();
        let mut source_bases = views
            .source_bases
            .values()
            .map(|(_, repository)| repository.clone())
            .collect::<Vec<_>>();
        source_bases.sort();
        let mut baseline = (*views.baseline).clone();
        for panes in baseline.values_mut() {
            panes.retain(|pane| !ignored.contains(&pane.id));
        }
        (
            baseline,
            (*views.initialized).clone(),
            pending,
            transfers,
            source_bases,
            views.attached.clone(),
            views.seeding.clone(),
            views.seed_attempts.keys().cloned().collect::<HashSet<_>>(),
            views.default_requests.clone(),
            views.fenced_closes.clone(),
            views.removals.keys().copied().collect::<HashSet<_>>(),
        )
    }

    /// A complete save publishes, from the kept discovery and with its
    /// settled scopes left out, exactly what a complete projection of every
    /// scope publishes from the same state, across randomized edits of what
    /// discovery reads and of what panes are made of.
    #[test]
    fn settled_publication_agrees_with_a_complete_projection_across_random_edits() {
        let (mut compared, mut settled, mut reached) = (0, 0, 0);
        for seed in 0..10 {
            let (mut app, _, _directory) = large_workspace(&SMALL);
            let (tx, requests) = crate::app::workspace_model_sync::test_model_channel();
            app.workspace_model_tx = tx;
            let mut rng = Rng(1_000 + seed);
            for step in 0..120 {
                let edit = if rng.below(2) == 0 {
                    edit(&mut app, &mut rng)
                } else {
                    pane_edit(&mut app, &mut rng)
                };
                if rng.below(4) == 0 {
                    settle_seeds(&mut app);
                }
                // What the edits published themselves.
                while requests.try_recv().is_ok() {}
                let run = || {
                    // The complete projection publishes from a copy.
                    let copy = app.shared_views.publication_copy();
                    let original = std::mem::replace(&mut app.shared_views, copy);
                    let complete = app.local_views();
                    app.publish_projected_view_changes(complete, ViewProjection::All);
                    let expected = published(&requests);
                    let complete = std::mem::replace(&mut app.shared_views, original);
                    let before = app.shared_views.settled_publications;
                    let reached = app.shared_views.transfer_unsettled;
                    app.publish_view_changes();
                    assert_eq!(published(&requests), expected, "published edits");
                    assert_eq!(
                        publication_state(&app.shared_views),
                        publication_state(&complete),
                        "publication state"
                    );
                    (
                        app.shared_views.settled_publications - before,
                        app.shared_views.transfer_unsettled - reached,
                    )
                };
                match std::panic::catch_unwind(std::panic::AssertUnwindSafe(run)) {
                    Ok((count, transfers)) => {
                        settled += count;
                        reached += transfers;
                    }
                    Err(panic) => {
                        eprintln!("seed {seed}, step {step}: after {edit}");
                        std::panic::resume_unwind(panic);
                    }
                }
                compared += 1;
            }
        }
        eprintln!(
            "{settled} scopes settled over {compared} publications, {reached} reached by a transfer"
        );
        assert!(settled > compared, "{settled} over {compared}");
        assert!(reached > 0, "no transfer reached a settled scope");
    }

    /// A complete reconciliation takes each projected scope's panes as its
    /// baseline and publishes against that. From the kept discovery, with
    /// its settled scopes left alone, it publishes exactly what it does
    /// when it projects and copies every scope, across randomized edits.
    #[test]
    fn reconciled_publication_agrees_with_a_complete_projection_across_random_edits() {
        let (mut compared, mut settled, mut ambiguous) = (0, 0, 0);
        for seed in 0..10 {
            let (mut app, _, _directory) = large_workspace(&SMALL);
            let (tx, requests) = crate::app::workspace_model_sync::test_model_channel();
            app.workspace_model_tx = tx;
            let mut rng = Rng(2_000 + seed);
            for step in 0..120 {
                let edit = if rng.below(2) == 0 {
                    edit(&mut app, &mut rng)
                } else {
                    pane_edit(&mut app, &mut rng)
                };
                if rng.below(4) == 0 {
                    settle_seeds(&mut app);
                }
                while requests.try_recv().is_ok() {}
                let run = || {
                    // The scopes a reconciliation projects: those listed
                    // and seeded.
                    let views = &app.shared_views;
                    let scopes = app
                        .workspace_model
                        .views
                        .iter()
                        .map(|view| view.scope.clone())
                        .filter(|scope| {
                            views.initialized.contains(scope)
                                && !views.seeding.contains(scope)
                                && !views.seed_failures.contains_key(scope)
                        })
                        .collect::<Vec<_>>();
                    // The exception the module documents: a view listed under
                    // another repository than the topology resolves for its
                    // path names that path with a second scope, and which of
                    // the two takes a default card's request depends on the
                    // order they were discovered in. Only in such a pass may
                    // that request go to a different scope.
                    let topology = app.borrowed_view_scope_topology();
                    let two_scopes = app.workspace_model.views.iter().any(|view| {
                        matches!(&view.scope, Scope::Worktree { repository, path }
                            if topology.authorities.repository_for(path)
                                .is_some_and(|resolved| resolved != repository))
                    });
                    drop(topology);
                    let copy = app.shared_views.publication_copy();
                    let original = std::mem::replace(&mut app.shared_views, copy);
                    let complete = app.local_views();
                    for scope in &scopes {
                        let mut panes = complete.panes.get(scope).cloned().unwrap_or_default();
                        panes.retain(|pane| !app.unpublished_chat(pane.id));
                        app.shared_views.baseline.insert(scope.clone(), panes);
                    }
                    app.publish_projected_view_changes(complete, ViewProjection::All);
                    let expected = published(&requests);
                    let complete = std::mem::replace(&mut app.shared_views, original);
                    let before = app.shared_views.settled_publications;
                    let kept = app.kept_local_views();
                    app.publish_reconciled_views(kept, ViewProjection::All, scopes);
                    let reconciled = published(&requests);
                    // There, only which scope requests a default card may
                    // differ; everything else must agree.
                    let ignored = if two_scopes {
                        inserted_if_empty(&reconciled)
                            .chain(inserted_if_empty(&expected))
                            .collect()
                    } else {
                        HashSet::new()
                    };
                    let differs = reconciled != expected;
                    assert_eq!(
                        published_without(reconciled, &ignored),
                        published_without(expected, &ignored),
                        "published edits"
                    );
                    assert_eq!(
                        publication_state_without(&app.shared_views, &ignored),
                        publication_state_without(&complete, &ignored),
                        "publication state"
                    );
                    (app.shared_views.settled_publications - before, differs)
                };
                match std::panic::catch_unwind(std::panic::AssertUnwindSafe(run)) {
                    Ok((count, differs)) => {
                        settled += count;
                        ambiguous += usize::from(differs);
                    }
                    Err(panic) => {
                        eprintln!("seed {seed}, step {step}: after {edit}");
                        std::panic::resume_unwind(panic);
                    }
                }
                compared += 1;
            }
        }
        eprintln!(
            "{settled} scopes settled over {compared} reconciled publications, \
             {ambiguous} requesting a default card from another scope"
        );
        assert!(settled > compared - ambiguous, "{settled} over {compared}");
        assert!(
            ambiguous < compared / 10,
            "{ambiguous} of {compared} differ"
        );
    }

    /// Settles every seed as the daemon would, acknowledging it and listing
    /// its view.
    fn settle_seeds(app: &mut Cagents) {
        for scope in app.shared_views.seeding_scopes() {
            app.acknowledge_view_edit(scope.clone(), None);
            if !app
                .workspace_model
                .views
                .iter()
                .any(|view| view.scope == scope)
            {
                let mut view = View::empty(ChannelId::new());
                view.scope = scope;
                view.revision = 1;
                app.workspace_model.views.push(view);
            }
        }
    }

    /// A settled workspace's worktree that holds no card yet, with the
    /// scope that would hold the card, and that scope's members.
    fn worktree_without_a_card(app: &mut Cagents) -> (SidebarAction, Scope) {
        app.persist();
        settle_seeds(app);
        app.persist();
        let project = &app.sidebar_model.projects[0];
        let row = project
            .worktrees
            .iter()
            .find(|row| {
                !app.durable.cards.iter().any(|card| {
                    crate::app::worktree_ui::card_directory(card) == Some(row.path.as_path())
                })
            })
            .expect("a worktree without a card");
        let action = SidebarAction::OpenWorktree {
            project_id: project.id,
            worktree_id: row.id,
            path: row.path.clone(),
        };
        let scope = Scope::Worktree {
            repository: project.repository.clone(),
            path: row.path.clone(),
        };
        (action, scope)
    }

    fn reset_discovery_counts(app: &Cagents) -> usize {
        app.shared_views.scope_discovery_count.set(0);
        app.shared_views.topology_build_count.set(0);
        app.shared_views.discovery.reused
    }

    fn assert_followed(app: &Cagents, reused: usize, scope: &Scope) {
        assert!(
            app.shared_views.discovery.reused > reused,
            "a pass followed"
        );
        assert_eq!(
            app.shared_views.scope_discovery_count.get(),
            0,
            "rediscovered"
        );
        assert_eq!(app.shared_views.topology_build_count.get(), 0, "topology");
        // A rediscovery of its own, once the counts are read.
        let views = app.local_views();
        assert!(
            views
                .cards
                .get(scope)
                .is_some_and(|cards| !cards.is_empty()),
            "{scope:?} holds the new card"
        );
    }

    /// Opening a worktree that holds no card makes its channel and puts a
    /// card there, and the saves around it discover nothing again.
    #[test]
    fn opening_a_worktree_without_a_card_discovers_nothing_again() {
        let (mut app, _, _directory) = large_workspace(&SMALL);
        let (action, _) = worktree_without_a_card(&mut app);
        let reused = reset_discovery_counts(&app);
        drop(app.update(Message::Sidebar(action)));
        drop(app.update(Message::NavigationSaveDue(app.navigation_save_generation)));
        app.persist();
        let channel = app.durable.active_channel.expect("the worktree's channel");
        assert_followed(&app, reused, &Scope::Channel(channel));
    }

    /// A card in a worktree nothing else discovers, no root nor listed view,
    /// adds its scope alone.
    #[test]
    fn a_card_in_an_undiscovered_worktree_adds_its_scope_alone() {
        let (mut app, _, _directory) = large_workspace(&SMALL);
        let (_, scope) = worktree_without_a_card(&mut app);
        let Scope::Worktree { path, .. } = &scope else {
            unreachable!();
        };
        let roots = app
            .known_worktree_roots
            .iter()
            .filter(|root| *root != path)
            .cloned()
            .collect();
        app.known_worktree_roots = roots;
        app.workspace_model.views.retain(|view| view.scope != scope);
        app.persist();
        let kept = |app: &Cagents| {
            let worktrees = app.shared_views.discovery.worktrees.as_ref().unwrap();
            worktrees.sources.contains_key(&scope)
        };
        assert!(!kept(&app), "nothing discovers the worktree yet");
        let reused = reset_discovery_counts(&app);
        let state = card("shell".into(), terminal(path), Existence::Open);
        Arc::make_mut(&mut app.durable).cards.push(state);
        app.persist();
        assert!(kept(&app), "the card discovers the worktree");
        assert_followed(&app, reused, &scope);
    }

    /// Settling more scopes than a journal holds between two passes makes
    /// the next discover every scope again, which then agrees with a
    /// rediscovery as every pass must.
    #[test]
    fn an_overflowing_journal_discovers_every_scope_again() {
        let (mut app, _, _directory) = large_workspace(&SMALL);
        app.persist();
        let kept = app.shared_views.discovery.worktrees.as_ref().unwrap();
        let unsettled = kept.sources.keys().take(3).cloned().collect::<Vec<_>>();
        assert_eq!(unsettled.len(), 3, "worktree scopes are kept");
        for scope in &unsettled {
            app.shared_views.baseline.remove(scope);
        }
        project(&mut app);
        app.shared_views.scope_discovery_count.set(0);
        let repository = app.sidebar_model.projects[0].repository.clone();
        let views = &mut app.shared_views;
        for index in 0..=settlement::JOURNAL_LIMIT {
            views.baseline.insert(
                Scope::Worktree {
                    repository: repository.clone(),
                    path: PathBuf::from(format!("/elsewhere/{index}")),
                },
                Vec::new(),
            );
        }
        // Past the journal's end, so only a rediscovery sees them settle.
        for scope in unsettled {
            views.initialize(scope, Vec::new());
        }
        project(&mut app);
        assert_eq!(app.shared_views.scope_discovery_count.get(), 1);
        project(&mut app);
        assert_eq!(
            app.shared_views.scope_discovery_count.get(),
            1,
            "then followed"
        );
    }

    /// The save a channel click defers reuses what the last save discovered,
    /// in a workspace with thousands of scopes.
    #[test]
    fn a_navigation_save_reuses_the_discovered_scopes() {
        let (mut app, channels, _directory) = large_workspace(&SMALL);
        let project = app.durable.channels[0].project_id;
        let same_project = app
            .durable
            .channels
            .iter()
            .filter(|channel| channel.project_id == project)
            .map(|channel| channel.id)
            .collect::<Vec<_>>();
        assert!(same_project.len() > 1, "{channels:?}");
        for &channel_id in same_project.iter().cycle().take(6) {
            drop(app.update(Message::Sidebar(SidebarAction::OpenChannel { channel_id })));
            drop(app.update(Message::NavigationSaveDue(app.navigation_save_generation)));
        }
        app.shared_views.scope_discovery_count.set(0);
        let reused = app.shared_views.discovery.reused;
        for &channel_id in same_project.iter().cycle().take(4) {
            drop(app.update(Message::Sidebar(SidebarAction::OpenChannel { channel_id })));
            drop(app.update(Message::NavigationSaveDue(app.navigation_save_generation)));
            assert!(app.navigation_save.is_none(), "the save ran");
        }
        assert_eq!(app.shared_views.scope_discovery_count.get(), 0);
        assert!(app.shared_views.discovery.reused >= reused + 4);
    }

    /// A structural pass shares the kept topology, which a root learned since
    /// replaces before the pass resolves any directory with it.
    #[test]
    fn a_structural_pass_resolves_with_a_root_learned_since_the_topology_was_kept() {
        use crate::app::worktree_ui::ResolveRoot;
        let (mut app, _, directory) = large_workspace(&SMALL);
        app.prime_view_discovery();
        let root = directory.path().join("learned-root");
        let inside = root.join("src");
        app.shared_views.topology_build_count.set(0);
        let kept = app.kept_view_topology();
        assert_eq!(app.shared_views.topology_build_count.get(), 0, "kept");
        assert_ne!(
            kept.scope_topology().roots.resolve(&inside),
            Some(root.as_path())
        );
        drop(kept);

        app.known_worktree_roots.insert(root.clone());
        let kept = app.kept_view_topology();
        assert_eq!(app.shared_views.topology_build_count.get(), 1);
        assert_eq!(
            kept.scope_topology().roots.resolve(&inside),
            Some(root.as_path())
        );
        // The save after the pass refreshes the same topology, not another.
        drop(app.reconcile_shared_views());
        assert_eq!(app.shared_views.topology_build_count.get(), 1);
        assert!(Arc::ptr_eq(&kept, &app.kept_view_topology()));
    }
}
