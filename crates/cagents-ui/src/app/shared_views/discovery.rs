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
//! rediscovery would. Their order is not always the same: channels keep
//! theirs, but worktree scopes come in the order they were first discovered
//! since the last complete discovery, so one found since then comes last
//! wherever a rediscovery would place it. Publication does not depend on the
//! order of scopes: each scope's edits are its own, and transfers already go
//! out in map order.

use super::settlement::KeyChanges;
use super::*;
use crate::sidebar::{Revisioned, SidebarProject, next_revision};

#[derive(Default)]
pub(in crate::app) struct ViewDiscovery {
    topology: Option<Topology>,
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

/// What `borrowed_view_scope_topology` builds, owned.
struct Topology {
    revision: u64,
    roots_generation: u64,
    projects_revision: u64,
    /// Each project's repository and worktree paths, in sidebar order, for
    /// when the projects took a revision without changing them.
    projects: Vec<(PathBuf, Vec<PathBuf>)>,
    repositories: HashMap<PathBuf, PathBuf>,
    project_repositories: HashSet<PathBuf>,
    roots: HashSet<PathBuf>,
}

impl Topology {
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

    fn authorities(&self) -> ViewScopeAuthorities<'_> {
        ViewScopeAuthorities::Indexed {
            repositories: &self.repositories,
            projects: &self.project_repositories,
        }
    }

    fn worktree_scope(&self, path: &Path) -> Option<Scope> {
        let repository = self.authorities().repository_for(path)?.to_owned();
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
            Scope::Worktree { repository, .. }
                if topology.authorities().contains_repository(repository) =>
            {
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
        let kept_topology = discovery
            .topology
            .as_ref()
            .map(|topology| topology.revision);
        let revision = self.refresh_view_topology(discovery);
        let channel_cards = self
            .durable
            .channels
            .iter()
            .flat_map(|channel| channel.cards.iter().copied())
            .collect::<HashSet<_>>();
        let topology = discovery.topology.as_ref().expect("refreshed above");
        self.refresh_card_worktrees(&mut discovery.cards, topology, &channel_cards);
        let followed = match discovery.worktrees.as_mut() {
            Some(worktrees) => {
                self.follow_worktrees(worktrees, topology, &mut discovery.cards, &settlement)
            }
            None => Err("none kept"),
        };
        tracing::debug!(target: "cagents::workspace_pipeline", stage = "kept_discovery",
            topology_kept = kept_topology == Some(revision),
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
        let worktree_cards = self
            .durable
            .cards
            .iter()
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
            if card.existence.is_open()
                && !card.kind.is_utility()
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
            },
            phase,
        );
        #[cfg(test)]
        if !discovery.unchecked {
            self.assert_rediscovery_agrees(&projected);
        }
        projected
    }

    /// The topology revision, rebuilding the topology if the projects or the
    /// known roots differ from those it was built from.
    fn refresh_view_topology(&self, discovery: &mut ViewDiscovery) -> u64 {
        let roots_generation = self.known_worktree_roots.generation();
        let projects = &self.sidebar_model.projects;
        let projects_revision = Revisioned::revision(projects);
        if let Some(topology) = &mut discovery.topology
            && topology.roots_generation == roots_generation
            && (topology.projects_revision == projects_revision || topology.lists(projects))
        {
            topology.projects_revision = projects_revision;
            return topology.revision;
        }
        #[cfg(test)]
        self.record_topology_build();
        let mut repositories = HashMap::new();
        for (path, repository) in self.view_repository_paths() {
            repositories
                .entry(path.to_owned())
                .or_insert_with(|| repository.to_owned());
        }
        let topology = Topology {
            revision: next_revision(),
            roots_generation,
            projects_revision,
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
            repositories,
            project_repositories: projects
                .iter()
                .map(|project| project.repository.clone())
                .collect(),
            roots: self.worktree_root_paths().into_iter().collect(),
        };
        let revision = topology.revision;
        discovery.topology = Some(topology);
        revision
    }

    /// Resolves again each card whose directory, owner, filing or channel
    /// changed, or that is new, and forgets the cards that are gone.
    fn refresh_card_worktrees(
        &self,
        cards: &mut CardWorktrees,
        kept: &Topology,
        channel_cards: &HashSet<CardId>,
    ) {
        let topology = kept.revision;
        cards.pass = cards.pass.wrapping_add(1);
        let pass = cards.pass;
        let mut roots = None;
        for card in &self.durable.cards {
            let channeled = channel_cards.contains(&card.id);
            let owner = self.worktree_owners.get(&card.id).map(PathBuf::as_path);
            let filed = self.durable.card_scopes.get(&card.id).map(PathBuf::as_path);
            let directory = worktree_ui::card_directory(card);
            if let Some(entry) = cards.cards.get_mut(&card.id)
                && entry.resolved_from(topology, channeled, owner, filed, directory)
            {
                entry.pass = pass;
                continue;
            }
            let roots =
                roots.get_or_insert_with(|| worktree_ui::WorktreeRoots::indexed(&kept.roots));
            let worktree = (!channeled)
                .then(|| self.unchanneled_card_worktree_indexed(card, roots))
                .flatten()
                .map(Path::to_owned);
            cards.count(worktree.as_deref(), true);
            let previous = cards.cards.insert(
                card.id,
                CardWorktree {
                    pass,
                    topology,
                    channeled,
                    owner: owner.map(Path::to_owned),
                    filed: filed.map(Path::to_owned),
                    directory: directory.map(Path::to_owned),
                    worktree,
                },
            );
            if let Some(previous) = previous {
                cards.count(previous.worktree.as_deref(), false);
            }
        }
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
    #[cfg(test)]
    fn assert_rediscovery_agrees(&self, projected: &LocalViews) {
        let counters = &self.shared_views;
        let saved = (
            counters.local_projection_count.get(),
            counters.topology_build_count.get(),
            counters.scope_discovery_count.get(),
        );
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
        assert_eq!(projected.panes, complete.panes, "scope panes");
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

    /// Paths a step can name: listed worktrees and repositories, a directory
    /// inside a root, and directories no root or project knows.
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
        match rng.below(24) {
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
                let (a, b) = (rng.below(cards.len()), rng.below(cards.len()));
                cards.swap(a, b);
                "reorder cards"
            }
            17 => {
                let views = &mut app.workspace_model.views;
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
}
