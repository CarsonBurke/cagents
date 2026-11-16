//! Scope discovery kept between complete saves.
//!
//! A complete save publishes the scopes this window shows, but finding them
//! meant rediscovering every scope it ever showed: every listed worktree
//! view, every known root and every card's worktree, in a workspace that
//! accumulates thousands of each. A pass now reuses three results, each kept
//! with the inputs it was derived from and derived again when one differs:
//!
//! - the scope topology, from the sidebar's projects and the known roots;
//! - each card's worktree, from the card's directory, owner and filing;
//! - the discovered scopes, and which of them are not yet seeded or settled.
//!
//! The active workspace is left out of the kept scopes, since navigation
//! changes it on most saves; each pass offers its scope on its own.
//!
//! The inputs are compared by value, or by a generation their owner draws
//! afresh on every change, never by counting on each writer to report one.
//! Everything else a projection reads, the publishable filter included, is
//! read afresh on every pass, so the scopes a pass publishes are the ones a
//! complete rediscovery would, in the same order.

use super::*;
use crate::model::ChannelId;
use crate::sidebar::SidebarProject;

#[derive(Default)]
pub(in crate::app) struct ViewDiscovery {
    topology: Option<Topology>,
    cards: CardWorktrees,
    scopes: Option<Discovered>,
    /// Passes that reused the discovered scopes, so tests can tell the
    /// comparison with a complete rediscovery exercised the kept ones.
    #[cfg(test)]
    pub(super) reused: usize,
    /// Leaves out that comparison, for timing workloads that measure a pass.
    #[cfg(test)]
    pub(super) unchecked: bool,
}

/// What `borrowed_view_scope_topology` builds, owned.
struct Topology {
    generation: u64,
    roots_generation: u64,
    /// Each project's repository and worktree paths, in sidebar order.
    projects: Vec<(PathBuf, Vec<PathBuf>)>,
    repositories: HashMap<PathBuf, PathBuf>,
    project_repositories: HashSet<PathBuf>,
    roots: HashSet<PathBuf>,
}

impl Topology {
    fn built_from(&self, roots_generation: u64, projects: &[SidebarProject]) -> bool {
        self.roots_generation == roots_generation
            && self.projects.len() == projects.len()
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
}

/// Each card's worktree, as `unchanneled_card_worktree_indexed` resolves it
/// for a card no channel lists, with what it was resolved from.
#[derive(Default)]
struct CardWorktrees {
    cards: HashMap<CardId, CardWorktree>,
    /// How many cards resolve to each worktree.
    paths: HashMap<PathBuf, usize>,
    /// Drawn afresh whenever a worktree gains its first card or loses its
    /// last, which is when the discovered scopes can change.
    generation: u64,
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
                self.generation = next_generation();
            }
        } else if let Some(count) = self.paths.get_mut(worktree) {
            *count -= 1;
            if *count == 0 {
                self.paths.remove(worktree);
                self.generation = next_generation();
            }
        }
    }
}

/// The discovered scopes, kept with the inputs that decide them.
struct Discovered {
    topology: u64,
    cards: u64,
    initialized: u64,
    baseline: u64,
    channels: Vec<ChannelId>,
    views: Vec<Scope>,
    scopes: Vec<Scope>,
    index: HashMap<Scope, usize>,
    /// The worktree scopes at each path. A listed view can name a path under
    /// another repository than the one its topology resolves.
    at_path: HashMap<PathBuf, Vec<usize>>,
    /// The scopes not yet seeded, or seeded with no settled baseline, which
    /// the publishable filter keeps whatever they hold.
    unsettled: Vec<usize>,
}

impl Discovered {
    fn derived_from(&self, app: &Cagents, topology: u64, cards: u64) -> bool {
        self.topology == topology
            && self.cards == cards
            && self.initialized == app.shared_views.initialized.generation()
            && self.baseline == app.shared_views.baseline.generation()
            && self.channels.iter().copied().eq(app
                .durable
                .channels
                .iter()
                .map(|channel| channel.id))
            && self
                .views
                .iter()
                .eq(app.workspace_model.views.iter().map(|view| &view.scope))
    }
}

impl Cagents {
    /// [`Self::local_views`], reusing what `discovery` kept from the last
    /// pass where its inputs are unchanged.
    pub(super) fn discovered_local_views(&self, discovery: &mut ViewDiscovery) -> LocalViews {
        let mut phase = WorkspaceProjectionTiming::new(true);
        let topology = self.refresh_view_topology(discovery);
        let channel_cards = self
            .durable
            .channels
            .iter()
            .flat_map(|channel| channel.cards.iter().copied())
            .collect::<HashSet<_>>();
        let kept = discovery.topology.as_ref().expect("refreshed above");
        self.refresh_card_worktrees(&mut discovery.cards, kept, &channel_cards);
        let cards = discovery.cards.generation;
        let reused = discovery
            .scopes
            .as_ref()
            .is_some_and(|scopes| scopes.derived_from(self, topology, cards));
        if reused {
            #[cfg(test)]
            {
                discovery.reused += 1;
            }
        } else {
            discovery.scopes = Some(self.discover_scopes(discovery, topology, cards));
        }
        let ViewDiscovery {
            topology: Some(topology),
            cards,
            scopes: Some(discovered),
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
        // Every scope the publishable filter can keep: those not settled,
        // those with published panes and those holding a card here. Channels
        // are few, so each is offered.
        let mut offered = discovered.unsettled.clone();
        offered.extend(
            self.durable
                .channels
                .iter()
                .filter_map(|channel| discovered.index.get(&Scope::Channel(channel.id))),
        );
        offered.extend(
            self.shared_views
                .baseline
                .iter()
                .filter(|(_, panes)| !panes.is_empty())
                .filter_map(|(scope, _)| discovered.index.get(scope)),
        );
        let mut held = HashSet::new();
        for (card, path) in &worktree_cards {
            if card.existence.is_open()
                && !card.kind.is_utility()
                && held.insert(*path)
                && let Some(indices) = discovered.at_path.get(*path)
            {
                offered.extend(indices);
            }
        }
        // A complete rediscovery lists the active workspace after the known
        // roots and before the cards' worktrees; one it does not otherwise
        // find is offered last, which offers the same set.
        let authorities = topology.authorities();
        let active = self.active_workspace.as_deref().and_then(|path| {
            let repository = authorities.repository_for(path)?;
            Some(Scope::Worktree {
                repository: repository.to_owned(),
                path: path.to_owned(),
            })
        });
        let active = match active.as_ref().map(|scope| discovered.index.get(scope)) {
            Some(Some(&index)) => {
                offered.push(index);
                None
            }
            _ => active,
        };
        offered.sort_unstable();
        offered.dedup();
        let scopes = offered
            .into_iter()
            .map(|index| Cow::Borrowed(&discovered.scopes[index]))
            .chain(active.map(Cow::Owned))
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

    /// The topology generation, rebuilding the topology if the projects or
    /// the known roots differ from those it was built from.
    fn refresh_view_topology(&self, discovery: &mut ViewDiscovery) -> u64 {
        let roots_generation = self.known_worktree_roots.generation();
        let projects = &self.sidebar_model.projects;
        if let Some(topology) = &discovery.topology
            && topology.built_from(roots_generation, projects)
        {
            return topology.generation;
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
            generation: next_generation(),
            roots_generation,
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
        let generation = topology.generation;
        discovery.topology = Some(topology);
        generation
    }

    /// Resolves again each card whose directory, owner, filing or channel
    /// changed, or that is new, and forgets the cards that are gone.
    fn refresh_card_worktrees(
        &self,
        cards: &mut CardWorktrees,
        kept: &Topology,
        channel_cards: &HashSet<CardId>,
    ) {
        let topology = kept.generation;
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
            let added = cards.cards[&card.id].worktree.clone();
            cards.count(added.as_deref(), true);
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

    fn discover_scopes(&self, discovery: &ViewDiscovery, topology: u64, cards: u64) -> Discovered {
        #[cfg(test)]
        self.shared_views
            .scope_discovery_count
            .set(self.shared_views.scope_discovery_count.get() + 1);
        let kept = discovery.topology.as_ref().expect("refreshed first");
        let worktrees = &discovery.cards.cards;
        let scopes = self
            .discovered_scopes(
                &kept.authorities(),
                None,
                self.durable
                    .cards
                    .iter()
                    .filter_map(|card| worktrees.get(&card.id)?.worktree.as_deref()),
            )
            .into_iter()
            .map(Cow::into_owned)
            .collect::<Vec<_>>();
        let index = scopes
            .iter()
            .enumerate()
            .map(|(index, scope)| (scope.clone(), index))
            .collect::<HashMap<_, _>>();
        let mut at_path = HashMap::<PathBuf, Vec<usize>>::new();
        for (index, scope) in scopes.iter().enumerate() {
            if let Scope::Worktree { path, .. } = scope {
                at_path.entry(path.clone()).or_default().push(index);
            }
        }
        let unsettled = scopes
            .iter()
            .enumerate()
            .filter(|(_, scope)| {
                !self.shared_views.initialized.contains(*scope)
                    || !self.shared_views.baseline.contains_key(*scope)
            })
            .map(|(index, _)| index)
            .collect();
        Discovered {
            topology,
            cards,
            initialized: self.shared_views.initialized.generation(),
            baseline: self.shared_views.baseline.generation(),
            channels: self
                .durable
                .channels
                .iter()
                .map(|channel| channel.id)
                .collect(),
            views: self
                .workspace_model
                .views
                .iter()
                .map(|view| view.scope.clone())
                .collect(),
            scopes,
            index,
            at_path,
            unsettled,
        }
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
    use crate::model::{Existence, ProjectChannel, Tombstone};
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
        drop(app.discovered_local_views(&mut discovery));
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
        match rng.below(17) {
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
        for seed in 0..12 {
            let (mut app, _, _directory) = large_workspace(&SMALL);
            let mut rng = Rng(seed);
            app.shared_views.discovery = ViewDiscovery::default();
            let mut reused = 0;
            for step in 0..120 {
                let before = app.shared_views.discovery.reused;
                let edit = edit(&mut app, &mut rng);
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
                // A new model replaces the kept discovery with it.
                reused += app.shared_views.discovery.reused.saturating_sub(before);
            }
            assert!(reused > 0, "seed {seed} reused no pass");
        }
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
