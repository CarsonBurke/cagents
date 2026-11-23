use super::*;
use crate::model::{Cards, ChannelId, WorktreeId};

/// Ownership comes from the launch recipe, never a shell's current directory or
/// OSC title. Component-aware matching also keeps `repo-fix` outside `repo`.
pub(super) fn worktree_root<'a>(
    directory: &'a Path,
    roots: impl Iterator<Item = &'a Path>,
) -> Option<&'a Path> {
    roots
        .filter(|root| directory.starts_with(root))
        .max_by_key(|root| root.components().count())
}

/// Resolves a directory to the worktree root holding it, a path that lives
/// as long as `'a` rather than as the resolver.
pub(super) trait ResolveRoot<'a> {
    fn resolve(&self, directory: &Path) -> Option<&'a Path>;
}

/// The nearest root among `directory`'s ancestors, found by looking each up
/// rather than comparing the directory with every historical checkout.
fn nearest_root<'a>(
    directory: &Path,
    root: impl Fn(&Path) -> Option<&'a Path>,
) -> Option<&'a Path> {
    directory
        .ancestors()
        .find_map(&root)
        .or_else(|| root(Path::new("")))
}

/// Resolve a directory by its ancestors rather than comparing it with every
/// historical checkout. Built for one immutable workspace projection.
pub(super) struct WorktreeRoots<'a> {
    roots: RootSet<'a>,
}

enum RootSet<'a> {
    Borrowed(HashSet<&'a Path>),
    /// A set kept between projections, which is not hashed again.
    Indexed(&'a HashSet<PathBuf>),
}

impl<'a> WorktreeRoots<'a> {
    pub(super) fn new(roots: impl Iterator<Item = &'a Path>) -> Self {
        Self {
            roots: RootSet::Borrowed(roots.collect()),
        }
    }

    pub(super) fn indexed(roots: &'a HashSet<PathBuf>) -> Self {
        Self {
            roots: RootSet::Indexed(roots),
        }
    }

    fn resolve(&self, directory: &Path) -> Option<&'a Path> {
        match &self.roots {
            RootSet::Borrowed(roots) => {
                nearest_root(directory, |ancestor| roots.get(ancestor).copied())
            }
            RootSet::Indexed(roots) => {
                let roots = *roots;
                nearest_root(directory, |ancestor| {
                    roots.get(ancestor).map(PathBuf::as_path)
                })
            }
        }
    }
}

impl<'a> ResolveRoot<'a> for WorktreeRoots<'a> {
    fn resolve(&self, directory: &Path) -> Option<&'a Path> {
        WorktreeRoots::resolve(self, directory)
    }
}

/// Every worktree root this window has known, each with its canonical form.
///
/// Pane ownership compares a pane's directory with these roots once
/// filesystem aliases are resolved. A long-lived window knows thousands of
/// roots, most of them deleted long ago, and resolving each one on every
/// worktree switch cost a `realpath` apiece on the UI thread. So a root is
/// resolved once, when it becomes known. One that could not be resolved, such
/// as a checkout that does not exist yet, keeps its own path. It is tried again
/// while the sidebar lists a row at that path, which is how a checkout created
/// later shows up. A deleted checkout gets no row and is not tried again. A
/// symbolic link retargeted under a root that already resolved goes unseen.
///
/// Roots are only added; [`Self::clear`] is for tests.
#[derive(Clone, Debug)]
pub(super) struct KnownWorktreeRoots {
    roots: HashSet<PathBuf>,
    canonical: CanonicalRoots,
    /// Drawn afresh for each new set and each added root, so equal
    /// generations mean the same roots. A copy keeps it with the roots.
    generation: u64,
}

impl Default for KnownWorktreeRoots {
    fn default() -> Self {
        Self {
            roots: HashSet::new(),
            canonical: CanonicalRoots::default(),
            generation: crate::revision::next_revision(),
        }
    }
}

#[derive(Clone, Debug, Default)]
struct CanonicalRoots {
    /// Each resolved root under its canonical form, and each unresolved root
    /// under its own path. Aliases of one directory share an entry.
    roots: HashMap<PathBuf, Vec<PathBuf>>,
    /// Roots known since the last resolution.
    pending: Vec<PathBuf>,
    /// Roots that could not be resolved.
    unresolved: HashSet<PathBuf>,
    /// The unresolved roots that a row lists, tried again on each pass until
    /// their checkout exists.
    listed: Vec<PathBuf>,
    /// The [`WorktreePathIndex::generation`] that `listed` was taken from.
    checked_rows: Option<u64>,
    /// Whether the forms were taken for a remote device, where paths are
    /// never resolved here.
    remote: bool,
    /// Counts resolutions, for tests to tell a cached form from a fresh one.
    #[cfg(test)]
    resolutions: usize,
}

impl KnownWorktreeRoots {
    /// Adds `root`, returning whether it was new.
    pub(super) fn insert(&mut self, root: PathBuf) -> bool {
        if self.roots.contains(&root) {
            return false;
        }
        self.canonical.pending.push(root.clone());
        self.generation = crate::revision::next_revision();
        self.roots.insert(root)
    }

    pub(super) fn generation(&self) -> u64 {
        self.generation
    }

    /// Adds every path in `roots`, copying only those not yet known.
    pub(super) fn extend_from_paths<'a>(&mut self, roots: impl IntoIterator<Item = &'a Path>) {
        for root in roots {
            if !self.roots.contains(root) {
                self.insert(root.to_owned());
            }
        }
    }

    pub(super) fn contains(&self, root: &Path) -> bool {
        self.roots.contains(root)
    }

    /// Whether `root` is known to be the directory whose canonical form is
    /// `form`, as resolved by the last [`Cagents::refresh_worktree_owners`].
    pub(super) fn is_alias(&self, form: &Path, root: &Path) -> bool {
        self.canonical
            .roots
            .get(form)
            .is_some_and(|aliases| aliases.iter().any(|alias| alias == root))
    }

    pub(super) fn iter(&self) -> std::collections::hash_set::Iter<'_, PathBuf> {
        self.roots.iter()
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.roots.len()
    }

    #[cfg(test)]
    pub(super) fn clear(&mut self) {
        *self = Self::default();
    }

    /// Resolves the roots known since the last pass, and those still
    /// unresolved that `rows` lists.
    fn resolve_new(&mut self, rows: &WorktreePathIndex) {
        let remote = crate::device::is_remote();
        let canonical = &mut self.canonical;
        if canonical.remote != remote {
            *canonical = CanonicalRoots {
                pending: self.roots.iter().cloned().collect(),
                remote,
                ..CanonicalRoots::default()
            };
        }
        let resolve = |root: &Path| {
            if remote {
                Some(root.to_owned())
            } else {
                root.canonicalize().ok()
            }
        };
        for root in std::mem::take(&mut canonical.pending) {
            #[cfg(test)]
            {
                canonical.resolutions += 1;
            }
            let form = resolve(&root).unwrap_or_else(|| {
                canonical.unresolved.insert(root.clone());
                root.clone()
            });
            canonical.roots.entry(form).or_default().push(root);
        }
        if canonical.checked_rows != Some(rows.generation()) {
            canonical.checked_rows = Some(rows.generation());
            canonical.listed = canonical
                .unresolved
                .iter()
                .filter(|root| rows.lists(root))
                .cloned()
                .collect();
        }
        let mut listed = std::mem::take(&mut canonical.listed);
        listed.retain(|root| {
            #[cfg(test)]
            {
                canonical.resolutions += 1;
            }
            let Some(form) = resolve(root) else {
                return true;
            };
            canonical.unresolved.remove(root);
            if let Some(aliases) = canonical.roots.get_mut(root) {
                aliases.retain(|alias| alias != root);
                if aliases.is_empty() {
                    canonical.roots.remove(root);
                }
            }
            canonical.roots.entry(form).or_default().push(root.clone());
            false
        });
        canonical.listed = listed;
    }

    /// The known root nearest above `directory`, whose canonical form is
    /// `resolved` when it could be resolved. Of two aliases of that root, the
    /// one `listed` is preferred.
    ///
    /// A directory that no longer exists keeps its own path, as a root deleted
    /// before it became known does. A root resolved while it existed is
    /// filed under its canonical form, which such a path need not start with,
    /// so the roots are also looked up by the paths they became known by.
    fn owner(
        &self,
        directory: &Path,
        resolved: Option<&Path>,
        listed: impl Fn(&Path) -> bool,
    ) -> Option<&Path> {
        let by_form = |ancestor: &Path| {
            self.canonical
                .roots
                .get(ancestor)?
                .iter()
                .max_by_key(|root| listed(root))
                .map(PathBuf::as_path)
        };
        match resolved {
            Some(canonical) => nearest_root(canonical, by_form),
            None => nearest_root(directory, |ancestor| {
                by_form(ancestor).or_else(|| self.roots.get(ancestor).map(PathBuf::as_path))
            }),
        }
    }
}

impl Extend<PathBuf> for KnownWorktreeRoots {
    fn extend<I: IntoIterator<Item = PathBuf>>(&mut self, roots: I) {
        for root in roots {
            self.insert(root);
        }
    }
}

impl FromIterator<PathBuf> for KnownWorktreeRoots {
    fn from_iter<I: IntoIterator<Item = PathBuf>>(roots: I) -> Self {
        let mut known = Self::default();
        known.extend(roots);
        known
    }
}

impl<'a> IntoIterator for &'a KnownWorktreeRoots {
    type Item = &'a PathBuf;
    type IntoIter = std::collections::hash_set::Iter<'a, PathBuf>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

/// The worktree rows' paths, indexed for `refresh_sidebar_worktree_panes`,
/// which runs after nearly every update. Hashing a path walks its components,
/// so with a long worktree list, building these maps on each pass cost more
/// than the rest of the update together. They are kept until the known roots
/// or the rows they were built from change, which comparing ids and path
/// bytes detects for a small fraction of that cost.
#[derive(Default)]
pub(super) struct WorktreePathIndex {
    /// What the maps were built from, in the order `path_sources` yields it.
    built_from: Vec<(PathSource, PathBuf)>,
    /// The known roots and every worktree row's path.
    roots: HashSet<PathBuf>,
    /// The first loaded row at each path, which attention is filed under.
    first_row: HashMap<PathBuf, (ProjectId, WorktreeId)>,
    /// Every worktree row at each path: a path listed under two projects is a
    /// row of both.
    rows: HashMap<PathBuf, Vec<(ProjectId, WorktreeId)>>,
    /// Counts rebuilds, so what is derived from the maps can tell they moved.
    generation: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PathSource {
    Known,
    Folder(ProjectId, WorktreeId),
    Worktree(ProjectId, WorktreeId),
}

fn path_sources<'a>(
    known: &'a KnownWorktreeRoots,
    projects: &'a [SidebarProject],
) -> impl Iterator<Item = (PathSource, &'a Path)> {
    let known = known.iter().map(|root| (PathSource::Known, root.as_path()));
    let rows = projects.iter().flat_map(|project| {
        let folder = project.folder_key().map(|key| {
            (
                PathSource::Folder(key.project_id, key.worktree_id),
                project.repository.as_path(),
            )
        });
        folder
            .into_iter()
            .chain(project.worktrees.iter().map(|worktree| {
                (
                    PathSource::Worktree(project.id, worktree.id),
                    worktree.path.as_path(),
                )
            }))
    });
    known.chain(rows)
}

impl WorktreePathIndex {
    /// Rebuilds the maps if the known roots or the rows changed.
    pub(super) fn refresh(&mut self, known: &KnownWorktreeRoots, projects: &[SidebarProject]) {
        let unchanged = self
            .built_from
            .iter()
            .map(|(source, path)| (*source, path.as_os_str()))
            .eq(path_sources(known, projects).map(|(source, path)| (source, path.as_os_str())));
        if unchanged {
            return;
        }
        self.generation += 1;
        self.built_from = path_sources(known, projects)
            .map(|(source, path)| (source, path.to_owned()))
            .collect();
        self.roots.clear();
        self.first_row.clear();
        self.rows.clear();
        for (source, path) in &self.built_from {
            match *source {
                PathSource::Known => {
                    self.roots.insert(path.clone());
                }
                PathSource::Folder(project, worktree) => {
                    self.first_row
                        .entry(path.clone())
                        .or_insert((project, worktree));
                }
                PathSource::Worktree(project, worktree) => {
                    self.roots.insert(path.clone());
                    self.first_row
                        .entry(path.clone())
                        .or_insert((project, worktree));
                    self.rows
                        .entry(path.clone())
                        .or_default()
                        .push((project, worktree));
                }
            }
        }
    }

    /// The first loaded row, a project folder's or a worktree's, at `path`.
    fn first_row(&self, path: &Path) -> Option<(ProjectId, WorktreeId)> {
        self.first_row.get(path).copied()
    }

    /// Whether Projects lists a row, a project folder's or a worktree's, at
    /// `path`: what `SidebarModel::worktree_at` finds, without scanning every
    /// row.
    pub(super) fn lists(&self, path: &Path) -> bool {
        self.first_row.contains_key(path)
    }

    /// Counts the rebuilds of the maps.
    pub(super) fn generation(&self) -> u64 {
        self.generation
    }

    /// The project whose root or worktree holds `directory`.
    pub(super) fn project_of(&self, directory: &Path) -> Option<ProjectId> {
        directory
            .ancestors()
            .find_map(|ancestor| self.first_row.get(ancestor))
            .map(|(project, _)| *project)
    }

    /// Every worktree row at `path`.
    pub(super) fn rows(&self, path: &Path) -> &[(ProjectId, WorktreeId)] {
        self.rows.get(path).map_or(&[], Vec::as_slice)
    }
}

impl<'a> ResolveRoot<'a> for &'a WorktreePathIndex {
    fn resolve(&self, directory: &Path) -> Option<&'a Path> {
        let index: &'a WorktreePathIndex = self;
        nearest_root(directory, |ancestor| {
            index.roots.get(ancestor).map(PathBuf::as_path)
        })
    }
}

/// Missing directories are not navigable worktrees. A permission or transient
/// I/O failure is not evidence of deletion, so retain those rows for recovery.
pub(super) fn worktree_directory_available(path: &Path) -> bool {
    if crate::device::is_remote() {
        return true;
    }
    match path.metadata() {
        Ok(metadata) => metadata.is_dir(),
        Err(error) => !matches!(
            error.kind(),
            std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
        ),
    }
}

/// The durable record of a worktree as Git lists it.
pub(super) fn worktree_state(
    project: ProjectId,
    record: &WorktreeRecord,
    existence: Existence,
) -> WorktreeState {
    WorktreeState {
        id: record.id,
        project_id: project,
        generation: 0,
        path: record.path.clone(),
        branch: record.branch.as_deref().map(|branch| {
            branch
                .strip_prefix("refs/heads/")
                .unwrap_or(branch)
                .to_owned()
        }),
        existence,
    }
}

/// How a record closes when its checkout left the disk by some other hand: a
/// `git worktree remove` in a terminal, or an agent cleaning up after itself.
/// Git listing the path again reopens the record.
pub(super) fn removed_outside() -> Existence {
    project_ui::removed_existence("worktree removed outside the application")
}

/// Close the restored records whose checkout is gone, returning the ones that
/// changed so they can be saved. Without this a record outlives its worktree
/// indefinitely, and every start looks for each of them on disk again. The
/// closed records still seed ownership boundaries: a session that survives
/// its worktree must not fall into the repository that contained it.
pub(super) fn retire_missing_worktrees(worktrees: &mut [WorktreeState]) -> Vec<WorktreeState> {
    worktrees
        .iter_mut()
        .filter(|worktree| {
            worktree.existence.is_open() && !worktree_directory_available(&worktree.path)
        })
        .map(|worktree| {
            worktree.existence = removed_outside();
            worktree.clone()
        })
        .collect()
}

pub(super) fn card_directory(card: &CardState) -> Option<&Path> {
    match &card.kind {
        CardKind::Terminal(session) => Some(&session.launch.working_directory),
        CardKind::Chat(session) => Some(&session.launch.recipe.working_directory),
        CardKind::Repository(view) => Some(&view.worktree),
        CardKind::PullRequests(view) => Some(&view.repository),
        CardKind::Browser(_)
        | CardKind::Settings
        | CardKind::Accounts
        | CardKind::Automations
        | CardKind::Work => None,
    }
}

/// A project's main checkout: its primary worktree, or the folder itself.
///
/// A submodule's or a `--separate-git-dir` checkout's repository is a Git
/// directory kept elsewhere, which names none of its rows. `git worktree
/// list` names the main checkout first, so the first row is that checkout;
/// the Git directory itself is never a place to work.
pub(super) fn project_main_checkout(project: &SidebarProject) -> PathBuf {
    let separate = project.kind != ProjectKind::Folder
        && project
            .repository
            .file_name()
            .is_some_and(|name| name != ".git");
    project
        .worktrees
        .iter()
        .find(|worktree| sidebar::is_primary_worktree(&project.repository, &worktree.path))
        .or_else(|| separate.then(|| project.worktrees.first()).flatten())
        .map(|worktree| worktree.path.clone())
        .unwrap_or_else(|| {
            project
                .repository
                .parent()
                .filter(|parent| sidebar::is_primary_worktree(&project.repository, parent))
                .unwrap_or(&project.repository)
                .to_path_buf()
        })
}

/// Whether `path` is still one of `project`'s worktrees. A channel's checkout
/// can be removed outside this window, and a channel never works in a
/// directory that no longer exists.
///
/// The registry records a checkout in its canonical form, which need not be
/// the spelling `git worktree list` gave the row, so a known alias of the row
/// matches too. The answer is the row's own spelling, the one panes and
/// strips are filed under.
pub(super) fn listed_worktree<'a>(
    project: &'a SidebarProject,
    known: &KnownWorktreeRoots,
    checkout: &Path,
) -> Option<&'a Path> {
    project
        .worktrees
        .iter()
        .map(|worktree| worktree.path.as_path())
        .find(|path| *path == checkout || known.is_alias(checkout, path))
}

pub(super) fn belongs_to_scope(
    card: &CardState,
    scope: &Path,
    roots: &[&Path],
    owners: &HashMap<CardId, PathBuf>,
) -> bool {
    card.existence.is_open()
        && owners.get(&card.id).map_or_else(
            || {
                card_directory(card).is_none_or(|directory| {
                    worktree_root(directory, roots.iter().copied()).unwrap_or(directory) == scope
                })
            },
            |owner| owner == scope,
        )
}

/// The pane a scope focuses when it opens: `preferred` while the scope still
/// holds it, else its first terminal or conversation, else its first editor.
pub(super) fn scope_focus<'a>(
    cards: impl Iterator<Item = &'a CardState> + Clone,
    preferred: Option<CardId>,
) -> Option<CardId> {
    let first = |kind: fn(&CardKind) -> bool| {
        cards
            .clone()
            .find(|card| kind(&card.kind))
            .map(|card| card.id)
    };
    preferred
        .filter(|id| cards.clone().any(|card| card.id == *id))
        .or_else(|| first(|kind| matches!(kind, CardKind::Terminal(_) | CardKind::Chat(_))))
        .or_else(|| first(|kind| matches!(kind, CardKind::Repository(_))))
        // A global-only strip still needs an active pane, without making that
        // browser an ownership source or displacing a worktree's own panes.
        .or_else(|| cards.clone().next().map(|card| card.id))
}

/// The cards some channel holds, which no worktree's strip shows.
fn channeled_cards(state: &WorkspaceState) -> HashSet<CardId> {
    state
        .channels
        .iter()
        .flat_map(|channel| channel.cards.iter().copied())
        .collect()
}

/// A view descriptor, not a replacement for the persisted workspace. Global
/// cards stay available; closing a card and hiding its worktree are unrelated.
fn scope_descriptor(
    state: &WorkspaceState,
    scope: &Path,
    roots: &[&Path],
    owners: &HashMap<CardId, PathBuf>,
    preferred_focus: Option<CardId>,
) -> WorkspaceState {
    let selected_channel = state
        .active_channel
        .and_then(|id| state.channels.iter().find(|channel| channel.id == id));
    let channel_cards = selected_channel.is_none().then(|| channeled_cards(state));
    let cards = state.cards.filtered(|card| {
        selected_channel.map_or_else(
            || {
                !channel_cards
                    .as_ref()
                    .is_some_and(|cards| cards.contains(&card.id))
                    && belongs_to_scope(card, scope, roots, owners)
            },
            |channel| card.existence.is_open() && channel.cards.contains(&card.id),
        )
    });
    let focused = scope_focus(cards.iter(), preferred_focus);
    WorkspaceState {
        schema_version: state.schema_version,
        id: state.id,
        generation: state.generation,
        existence: state.existence.clone(),
        cards,
        focused,
        active_worktree: None,
        worktree_focus: HashMap::new(),
        project_order: state.project_order.clone(),
        channels: Vec::new(),
        active_channel: None,
        // A projected strip is a view, never a place to file a card.
        card_scopes: HashMap::new(),
        chat_drafts: HashMap::new(),
        chat_draft_attachments: HashMap::new(),
        chat_draft_models: HashMap::new(),
        chat_waiting_messages: HashMap::new(),
        kept_chat_drafts: HashMap::new(),
        stopping_chats: Vec::new(),
        agent_updates: Default::default(),
        workspace_history: Vec::new(),
        repository_navigation: HashMap::new(),
        // A projection carries no channels, so it has nothing to say about what
        // was shared with which registry.
        workspace_model_identity: None,
        local_only_cards: HashSet::new(),
        pending_chat_admissions: Default::default(),
        pending_terminal_creations: HashSet::new(),
    }
}

fn insertion_indices(
    visible: &[WorkspaceCard],
    durable: &Cards,
    focused: Option<CardId>,
) -> (usize, usize) {
    let anchor = focused.and_then(|id| {
        let visible_index = visible.iter().position(|card| card.id == id)?;
        let durable_index = durable.iter().position(|card| card.id == id)?;
        Some((visible_index + 1, durable_index + 1))
    });
    anchor.unwrap_or((visible.len(), durable.len()))
}

impl Cagents {
    /// The mounted strip orders its channel; unmounted members retain their
    /// saved order until the strip has finished restoring them.
    pub(super) fn active_channel_order(&self) -> Option<(ChannelId, Vec<CardId>)> {
        let id = self.durable.active_channel?;
        let channel = self
            .durable
            .channels
            .iter()
            .find(|channel| channel.id == id)?;
        let members = channel.cards.iter().copied().collect::<HashSet<_>>();
        let mut order = self
            .workspace
            .order()
            .filter(|card| members.contains(card))
            .collect::<Vec<_>>();
        let mounted = order.iter().copied().collect::<HashSet<_>>();
        order.extend(
            channel
                .cards
                .iter()
                .copied()
                .filter(|card| !mounted.contains(card)),
        );
        Some((id, order))
    }

    /// Merge navigation into the full descriptor; never snapshot the projected
    /// strip over durable cards belonging to a hidden scope.
    ///
    /// Returns whether a channel's panes changed, in membership or order,
    /// which is what a shared view reads of this.
    pub(super) fn snapshot_workspace_scope(&mut self) -> bool {
        if self.active_workspace.is_none() {
            return false;
        }
        let focused = self.workspace.focused();
        let active_order = self.active_channel_order();
        if let Some(id) = self.durable.active_channel {
            self.channel_offsets
                .insert(id, self.workspace.camera().offset());
        } else if let Some(path) = &self.active_workspace {
            self.worktree_offsets
                .insert(path.clone(), self.workspace.camera().offset());
            if let Some(focused) = focused {
                self.worktree_focus.insert(path.clone(), focused);
            } else {
                self.worktree_focus.remove(path);
            }
        }
        let open = self
            .durable
            .cards
            .iter()
            .filter(|card| card.existence.is_open())
            .map(|card| card.id)
            .collect::<HashSet<_>>();
        let moved = active_order.as_ref().is_some_and(|(id, order)| {
            self.durable
                .channels
                .iter()
                .any(|channel| channel.id == *id && &channel.cards != order)
        }) || self
            .durable
            .channels
            .iter()
            .any(|channel| channel.cards.iter().any(|id| !open.contains(id)));
        let needs_update = moved
            || self.durable.active_worktree != self.active_workspace
            || self.durable.worktree_focus != self.worktree_focus
            || self.durable.channels.iter().any(|channel| {
                Some(channel.id) == self.durable.active_channel && channel.focused != focused
            });
        if !needs_update {
            return false;
        }
        let durable = Arc::make_mut(&mut self.durable);
        if let Some((id, order)) = active_order
            && let Some(channel) = durable.channels.iter_mut().find(|channel| channel.id == id)
        {
            channel.cards = order;
        }
        for channel in &mut durable.channels {
            channel.cards.retain(|id| open.contains(id));
            if !channel
                .focused
                .is_some_and(|id| channel.cards.contains(&id))
            {
                channel.focused = channel.cards.first().copied();
            }
        }
        if let Some(id) = durable.active_channel
            && let Some(channel) = durable.channels.iter_mut().find(|channel| channel.id == id)
        {
            channel.focused = focused.filter(|card| channel.cards.contains(card));
        }
        durable.active_worktree.clone_from(&self.active_workspace);
        durable.worktree_focus.clone_from(&self.worktree_focus);
        moved
    }

    pub(super) fn register_channel_card(&mut self, card: CardId) {
        let Some(id) = self.durable.active_channel else {
            // Browsers have no launch directory from which to infer a view.
            // Directory-bearing panes retain their explicit or inferred owner,
            // and utility panes follow the user's navigation between views.
            if self
                .card_state(card)
                .is_some_and(|state| matches!(state.kind, CardKind::Browser(_)))
                && !self.durable.card_scopes.contains_key(&card)
                && let Some(path) = &self.active_workspace
            {
                Arc::make_mut(&mut self.durable)
                    .card_scopes
                    .insert(card, path.clone());
                self.worktree_owners.insert(card, path.clone());
            }
            return;
        };
        let durable = Arc::make_mut(&mut self.durable);
        if let Some(channel) = durable.channels.iter_mut().find(|channel| channel.id == id) {
            if !channel.cards.contains(&card) {
                channel.cards.push(card);
            }
            channel.focused = Some(card);
        }
    }

    /// The exact directory a channel enters, where its new panes start, also
    /// used by navigation admission: the channel's checkout while it is still
    /// one of its project's worktrees, and otherwise the project's main
    /// checkout, so a channel whose worktree was removed still opens.
    pub(super) fn channel_directory(&self, id: ChannelId) -> Option<PathBuf> {
        let channel = self
            .durable
            .channels
            .iter()
            .find(|channel| channel.id == id)?;
        let project = self.sidebar_model.project(channel.project_id)?;
        Some(
            channel
                .checkout
                .as_deref()
                .and_then(|checkout| listed_worktree(project, &self.known_worktree_roots, checkout))
                .map_or_else(|| project_main_checkout(project), Path::to_path_buf),
        )
    }

    /// Shows channel `id`, saving the workspace now: callers may have changed
    /// it on the way here.
    pub(super) fn select_channel(&mut self, id: ChannelId) -> Task<Message> {
        self.show_channel(id, DurableChange::Content)
    }

    /// Shows channel `id` when nothing but where this window is changes, as
    /// when the user opens it from the sidebar. The switch is saved with the
    /// next save, or within [`NAVIGATION_SAVE_DELAY`].
    pub(super) fn navigate_to_channel(&mut self, id: ChannelId) -> Task<Message> {
        self.show_channel(id, DurableChange::Navigation)
    }

    pub(super) fn show_channel(&mut self, id: ChannelId, change: DurableChange) -> Task<Message> {
        let Some(path) = self.channel_directory(id) else {
            return Task::none();
        };
        let channel = self
            .durable
            .channels
            .iter()
            .find(|channel| channel.id == id)
            .expect("the channel directory resolved this same channel");
        if self.durable.active_channel == Some(id)
            && self.active_workspace.as_ref() == Some(&path)
            && self.workspace.order().eq(self
                .durable
                .cards
                .iter()
                .filter(|card| card.existence.is_open() && channel.cards.contains(&card.id))
                .map(|card| card.id))
        {
            // Choosing the channel already open still means going to it: the
            // keyboard returns to its focused pane unless something inside
            // that pane already holds it.
            return match self.workspace.focused() {
                Some(card) if self.card_holds_keyboard(card) => {
                    self.focus_pane_keyboard_after_press(card)
                }
                _ => Task::none(),
            };
        }
        self.materialize_replica_snapshot();
        self.snapshot_workspace_scope();
        let left = self
            .durable
            .active_channel
            .filter(|&previous| previous != id);
        let promoted = if self.durable.active_channel != Some(id) {
            self.discard_unused_default_chats()
        } else {
            false
        };
        Arc::make_mut(&mut self.durable).active_channel = Some(id);
        self.retire_left_channel(left);
        let focused = self
            .durable
            .channels
            .iter()
            .find(|channel| channel.id == id)
            .and_then(|channel| channel.focused);
        let scoped = scope_descriptor(&self.durable, &path, &[], &self.worktree_owners, focused);
        let offset = self.channel_offsets.get(&id).copied();
        let scope = self.install_workspace_scope(scoped, path, offset, change);
        if promoted {
            self.persist();
        }
        scope
    }

    /// Resolve filesystem aliases only when changing/restoring scope, never in
    /// the render path or by rewriting a hosted session's durable launch recipe.
    ///
    /// The known roots keep their canonical forms between calls, so this
    /// resolves only the directories of open panes and `selected`, not every
    /// worktree the window has known.
    pub(super) fn refresh_worktree_owners(&mut self, selected: Option<&Path>) -> Option<PathBuf> {
        // Deleted worktree rows disappear from navigation, not from ownership.
        // Otherwise a surviving nested session would fall into its parent repo.
        self.known_worktree_roots
            .extend_from_paths(self.sidebar_model.projects.iter().flat_map(|project| {
                project
                    .worktrees
                    .iter()
                    .map(|worktree| worktree.path.as_path())
            }));
        self.known_worktree_roots.extend_from_paths(
            self.sidebar_model
                .projects
                .iter()
                .filter(|project| project.kind == ProjectKind::Folder)
                .map(|project| project.repository.as_path()),
        );
        self.known_worktree_roots
            .extend_from_paths(
                self.durable
                    .cards
                    .iter()
                    .filter_map(|card| match &card.kind {
                        CardKind::Repository(view) => Some(view.worktree.as_path()),
                        _ => None,
                    }),
            );
        self.refresh_worktree_paths();
        self.known_worktree_roots.resolve_new(&self.worktree_paths);
        let index = &self.worktree_paths;
        let known = &self.known_worktree_roots;
        let remote = crate::device::is_remote();
        let resolve = |directory: &Path| {
            let resolved = if remote {
                Some(directory.to_path_buf())
            } else {
                directory.canonicalize().ok()
            };
            known
                .owner(directory, resolved.as_deref(), |root| index.lists(root))
                .map(Path::to_path_buf)
                .or(resolved)
                .unwrap_or_else(|| directory.into())
        };
        let mut directories = HashMap::<&Path, PathBuf>::new();
        self.worktree_owners = self
            .durable
            .cards
            .iter()
            .filter(|card| card.existence.is_open())
            .filter_map(|card| {
                // An explicit filing outranks directory discovery, including
                // cloned editors whose descriptors point into nested roots.
                if let Some(scope) = self.durable.card_scopes.get(&card.id) {
                    return Some((card.id, scope.clone()));
                }
                let directory = card_directory(card)?;
                let owner = directories
                    .entry(directory)
                    .or_insert_with(|| resolve(directory));
                Some((card.id, owner.clone()))
            })
            .collect();
        selected.map(resolve)
    }

    /// Rebuilds the index of the paths Projects lists, and places each
    /// inspected conversation's Work rows again when it changed, since
    /// which rows there are depends on it.
    fn refresh_worktree_paths(&mut self) {
        let generation = self.worktree_paths.generation();
        self.worktree_paths
            .refresh(&self.known_worktree_roots, &self.sidebar_model.projects);
        if self.worktree_paths.generation() != generation {
            self.place_inspected_work();
        }
    }

    /// Refresh which worktree rows hold an open pane after state changes,
    /// never during rendering.
    ///
    /// A pane belongs to the worktree `card_worktree` files it under, the path
    /// attribution agent updates use too, though every card kind counts here
    /// rather than only agents: any open pane is work in front of the user.
    /// Global cards (browsers) and channel cards are
    /// nobody's worktree, so neither counts. A path listed under two projects
    /// marks both rows.
    ///
    /// Structural updates also rebind existing activity and unread ownership.
    /// Root discovery is shared for this pass, and unchanged grid output skips
    /// it entirely.
    pub(super) fn refresh_sidebar_worktree_panes(&mut self) {
        // A passive Folder load has no worktree row and need not select its
        // scope. Admit its root before deriving ownership, including cached
        // attention, without requiring a navigation/read acknowledgement.
        // Resolve aliases and rebuild owners only when a new boundary appears;
        // unchanged output and ordinary structural passes retain the cache.
        if self.sidebar_model.projects.iter().any(|project| {
            project.kind == ProjectKind::Folder
                && !self.known_worktree_roots.contains(&project.repository)
        }) {
            #[cfg(test)]
            terminal_output_tests::record_folder_root_refresh();
            self.refresh_worktree_owners(None);
        }
        self.refresh_worktree_paths();
        let index = &self.worktree_paths;
        let channeled = channeled_cards(&self.durable);
        // One resolution per pane serves both occupancy and where the pane
        // rows place a pane that has nothing to show.
        let mut occupied = HashSet::new();
        let mut pane_checkouts = BTreeMap::new();
        for card in self
            .durable
            .cards
            .iter()
            .filter(|card| card.existence.is_open() && !channeled.contains(&card.id))
        {
            let Some(directory) = self.unchanneled_card_worktree_indexed(card, &index) else {
                continue;
            };
            occupied.insert(directory);
            if matches!(card.kind, CardKind::Terminal(_) | CardKind::Chat(_))
                && let Some(row) = index.first_row(directory)
            {
                pane_checkouts.insert(card.id, row);
            }
        }
        let worktrees = occupied
            .iter()
            .flat_map(|path| index.rows(path))
            .copied()
            .collect();
        // Attention chooses the first matching loaded row, unlike occupancy:
        // the latter deliberately marks every project sharing an exact path.
        // Folder boundaries were admitted above; their keys identify the
        // exact loaded row once the longest-root lookup resolves a directory.
        let attention = self
            .sidebar_state
            .attention_cards()
            .collect::<HashSet<_>>()
            .into_iter()
            .map(|id| {
                let owner = self
                    .card_state(id)
                    .filter(|card| {
                        card.existence.is_open()
                            && matches!(card.kind, CardKind::Terminal(_) | CardKind::Chat(_))
                            && !channeled.contains(&id)
                    })
                    .and_then(|card| self.unchanneled_card_worktree_indexed(card, &index))
                    .and_then(|directory| index.first_row(directory));
                (id, owner)
            })
            .collect::<Vec<_>>();
        self.sidebar_state.set_worktrees_with_panes(worktrees);
        self.sidebar_state.set_pane_checkouts(pane_checkouts);
        for (id, owner) in attention {
            self.sidebar_state.reattribute_card_attention(id, owner);
        }
        self.refresh_attention_projection();
    }

    /// The worktree `card` belongs to, if it is in no channel. This searches
    /// every channel for the card; a pass over many cards looks them up in
    /// [`channeled_cards`] and calls [`Self::unchanneled_card_worktree_indexed`].
    pub(super) fn card_worktree<'a>(&'a self, card: &'a CardState) -> Option<&'a Path> {
        #[cfg(test)]
        tests::CHANNEL_SEARCHES.set(tests::CHANNEL_SEARCHES.get() + 1);
        if self
            .durable
            .channels
            .iter()
            .any(|channel| channel.cards.contains(&card.id))
        {
            return None;
        }
        self.unchanneled_card_worktree(card)
    }

    /// `card_worktree` for a card the caller already knows is in no channel.
    pub(super) fn unchanneled_card_worktree<'a>(&'a self, card: &'a CardState) -> Option<&'a Path> {
        if let Some(scope) = self
            .worktree_owners
            .get(&card.id)
            .or_else(|| self.durable.card_scopes.get(&card.id))
        {
            return Some(scope.as_path());
        }
        let directory = card_directory(card)?;
        Some(
            worktree_root(
                directory,
                self.known_worktree_roots
                    .iter()
                    .map(PathBuf::as_path)
                    .chain(self.sidebar_model.projects.iter().flat_map(|project| {
                        project
                            .worktrees
                            .iter()
                            .map(|worktree| worktree.path.as_path())
                    })),
            )
            .unwrap_or(directory),
        )
    }

    pub(super) fn indexed_worktree_roots(&self) -> WorktreeRoots<'_> {
        WorktreeRoots::new(self.worktree_root_candidates())
    }

    /// The roots `indexed_worktree_roots` indexes, owned, for an index that
    /// must outlive mutable updates of this window.
    pub(super) fn worktree_root_paths(&self) -> Vec<PathBuf> {
        self.worktree_root_candidates()
            .map(Path::to_owned)
            .collect()
    }

    /// A digest of the worktree roots, independent of their order, that
    /// changes when one is added or removed.
    pub(super) fn worktree_roots_digest(&self) -> u64 {
        use std::hash::BuildHasher;
        let hasher =
            std::hash::BuildHasherDefault::<std::collections::hash_map::DefaultHasher>::default();
        self.worktree_root_candidates()
            .fold(0, |digest: u64, root| {
                digest.wrapping_add(hasher.hash_one(root))
            })
    }

    fn worktree_root_candidates(&self) -> impl Iterator<Item = &Path> {
        self.known_worktree_roots
            .iter()
            .map(PathBuf::as_path)
            .chain(self.sidebar_model.projects.iter().flat_map(|project| {
                project
                    .worktrees
                    .iter()
                    .map(|worktree| worktree.path.as_path())
            }))
    }

    pub(super) fn unchanneled_card_worktree_indexed<'a>(
        &'a self,
        card: &'a CardState,
        roots: &impl ResolveRoot<'a>,
    ) -> Option<&'a Path> {
        if let Some(scope) = self
            .worktree_owners
            .get(&card.id)
            .or_else(|| self.durable.card_scopes.get(&card.id))
        {
            return Some(scope);
        }
        let directory = card_directory(card)?;
        Some(roots.resolve(directory).unwrap_or(directory))
    }

    /// The directory a sidebar open of a project or worktree enters.
    pub(super) fn sidebar_entered_directory(&self, action: &SidebarAction) -> Option<PathBuf> {
        match action {
            SidebarAction::OpenRepository {
                project_id,
                repository,
            } => Some(self.project_entry_directory(*project_id, repository)),
            SidebarAction::OpenWorktree { path, .. } => Some(path.clone()),
            _ => None,
        }
    }

    /// A project opens in its main checkout, where its channels work by
    /// default.
    pub(super) fn project_entry_directory(&self, project: ProjectId, repository: &Path) -> PathBuf {
        self.sidebar_model
            .project(project)
            .map_or_else(|| repository.to_path_buf(), project_main_checkout)
    }

    pub(super) fn select_worktree(
        &mut self,
        path: PathBuf,
        create_if_empty: bool,
    ) -> Task<Message> {
        self.select_worktree_checked(path, create_if_empty).1
    }

    /// A synchronous success witness for callers that expose a new draft only
    /// after worktree protection and scope selection succeeded. A no-op selection
    /// is successful; a refused protection is not.
    pub(super) fn select_worktree_checked(
        &mut self,
        path: PathBuf,
        create_if_empty: bool,
    ) -> (bool, Task<Message>) {
        self.show_worktree(path, create_if_empty, DurableChange::Content)
    }

    /// Shows the worktree at `path` when nothing but where this window is
    /// changes, as when the user opens it from the sidebar. The switch is
    /// saved with the next save, or within [`NAVIGATION_SAVE_DELAY`]; a
    /// default pane opened for an empty worktree saves itself.
    pub(super) fn navigate_to_worktree(
        &mut self,
        path: PathBuf,
        create_if_empty: bool,
    ) -> Task<Message> {
        self.navigate_to_worktree_checked(path, create_if_empty).1
    }

    /// The navigation counterpart of [`Self::select_worktree_checked`], keeping
    /// its success witness so callers focus a pane only after scope selection
    /// succeeds.
    pub(super) fn navigate_to_worktree_checked(
        &mut self,
        path: PathBuf,
        create_if_empty: bool,
    ) -> (bool, Task<Message>) {
        self.show_worktree(path, create_if_empty, DurableChange::Navigation)
    }

    /// Whether the legacy strip of the checkout at `root` still holds an open
    /// pane outside every channel. Opening that checkout keeps showing the
    /// strip until such panes are moved into channels, so none of them
    /// becomes unreachable. Utility panes appear in every strip and count for
    /// none. `root` and the panes' owners come from the last
    /// [`Self::refresh_worktree_owners`].
    pub(super) fn legacy_strip_holds_panes(&self, root: &Path) -> bool {
        let channeled = channeled_cards(&self.durable);
        let roots = &self.worktree_paths;
        self.durable.cards.iter().any(|card| {
            card.existence.is_open()
                && !card.kind.is_utility()
                && !channeled.contains(&card.id)
                && self.unchanneled_card_worktree_indexed(card, &roots) == Some(root)
        })
    }

    pub(super) fn show_worktree(
        &mut self,
        path: PathBuf,
        create_if_empty: bool,
        change: DurableChange,
    ) -> (bool, Task<Message>) {
        let path = self
            .refresh_worktree_owners(Some(&path))
            .expect("selected scope");
        if let Err(error) = self.worktree_protections.protect_next(&path) {
            self.status = Some(TransientNotice::warning(format!(
                "Cannot open this worktree yet: {error}"
            )));
            return (false, Task::none());
        }
        let roots = self
            .known_worktree_roots
            .iter()
            .map(PathBuf::as_path)
            .collect::<Vec<_>>();
        // Each pane is looked up in the channels' members, collected once,
        // and in the root index refreshed with the owners above, rather than
        // searching every channel and every known root for it.
        let channeled = channeled_cards(&self.durable);
        let worktree_roots = &self.worktree_paths;
        let in_selected_worktree = |card: &CardState| {
            !channeled.contains(&card.id)
                && self.unchanneled_card_worktree_indexed(card, &worktree_roots)
                    == Some(path.as_path())
        };
        // Ownership and selected scope were resolved against the same canonical roots.
        let has_terminal = self.durable.cards.iter().any(|card| {
            card.existence.is_open()
                && matches!(&card.kind, CardKind::Terminal(_) | CardKind::Chat(_))
                && in_selected_worktree(card)
        });
        let was_channel = self.durable.active_channel.is_some();
        if self.active_workspace.as_ref() == Some(&path)
            && !was_channel
            && self.workspace.order().eq(self
                .durable
                .cards
                .iter()
                .filter(|card| {
                    !channeled.contains(&card.id)
                        && belongs_to_scope(card, &path, &roots, &self.worktree_owners)
                })
                .map(|card| card.id))
        {
            if !create_if_empty {
                return (true, Task::none());
            }
            return (
                true,
                if !has_terminal {
                    self.ensure_default_pane()
                } else {
                    Task::none()
                },
            );
        }
        let promoted = !was_channel
            && self.active_workspace.as_ref().is_some_and(|previous| {
                previous != &path
                    && self.durable.cards.iter().any(|card| {
                        card.existence.is_open()
                            && card_directory(card) == Some(previous.as_path())
                            && in_selected_worktree(card)
                    })
            });
        if !was_channel && let Some(previous) = &self.active_workspace {
            // Repository discovery can promote an unregistered launch directory
            // into its parent root. Carry its camera/focus into that same scope.
            let key = if promoted { &path } else { previous };
            self.worktree_offsets
                .insert(key.clone(), self.workspace.camera().offset());
            if let Some(focused) = self.workspace.focused() {
                self.worktree_focus.insert(key.clone(), focused);
            } else {
                self.worktree_focus.remove(key);
            }
        }
        drop(roots);
        self.materialize_replica_snapshot();
        if was_channel {
            self.snapshot_workspace_scope();
        }
        let promoted_default =
            if !promoted && (was_channel || self.active_workspace.as_ref() != Some(&path)) {
                self.discard_unused_default_chats()
            } else {
                false
            };
        let left = self.durable.active_channel;
        Arc::make_mut(&mut self.durable).active_channel = None;
        self.retire_left_channel(left);
        let roots = self
            .known_worktree_roots
            .iter()
            .map(PathBuf::as_path)
            .collect::<Vec<_>>();
        let scoped = scope_descriptor(
            &self.durable,
            &path,
            &roots,
            &self.worktree_owners,
            self.worktree_focus.get(&path).copied(),
        );
        let has_terminal = scoped
            .cards
            .iter()
            .any(|card| matches!(card.kind, CardKind::Terminal(_) | CardKind::Chat(_)));
        let offset = self.worktree_offsets.get(&path).copied();
        let scope = self.install_workspace_scope(scoped, path.clone(), offset, change);
        let terminal = if create_if_empty && !has_terminal {
            self.ensure_default_pane()
        } else {
            Task::none()
        };
        if promoted_default {
            self.persist();
        }
        (true, Task::batch([scope, terminal]))
    }

    fn install_workspace_scope(
        &mut self,
        scoped: WorkspaceState,
        path: PathBuf,
        offset: Option<f32>,
        change: DurableChange,
    ) -> Task<Message> {
        let width = self.workspace.viewport_width();
        // Hide before replacing the strip: its former widgets no longer get a
        // chance to send native-surface visibility updates after unmounting.
        // The warm chat budget waits for `apply_viewport` below, once the
        // panes this scope shows are visible again and so not candidates.
        self.apply_workspace_surface_visibility(false);
        self.active_workspace = Some(path);
        self.workspace_view_generation = self.workspace_view_generation.wrapping_add(1);
        // Iced inherits old anonymous IDs during diff. A named generation ID
        // forces a new scroll state so operations address this worktree's strip.
        self.workspace_scroll_id = widget::Id::new(format!(
            "workspace-strip-{}",
            self.workspace_view_generation
        ));
        self.workspace = Self::rebuild_workspace(&scoped);
        self.workspace.resize_viewport(width);
        self.rehide_closing_panes();
        // Focus selects the initial position only on the first visit. Returning
        // to a strip restores its actual offset, even if focus is offscreen.
        let offset = offset.unwrap_or_else(|| self.workspace.camera().target());
        self.workspace.native_scroll(offset, width);
        // The cards this strip shows, and those the strip being left showed.
        // Every surface was just hidden, so a card on screen both before and
        // after is told again to show itself. Every other card stays where
        // it was, offscreen.
        let transitions = self
            .durable
            .cards
            .iter()
            .filter(|card| card.existence.is_open())
            .filter_map(|card| {
                let from = match self.exposure(card.id) {
                    Exposure::Visible => VisibilityClass::Visible,
                    Exposure::Prefetch => VisibilityClass::Overscan,
                    Exposure::Offscreen => VisibilityClass::Offscreen,
                };
                let to = self
                    .workspace
                    .visibility(card.id)
                    .unwrap_or(VisibilityClass::Offscreen);
                (from != to || to != VisibilityClass::Offscreen).then_some(VisibilityTransition {
                    id: card.id,
                    from,
                    to,
                })
            })
            .collect();
        let viewport = self.apply_viewport(
            ViewportUpdate {
                offset_changed: true,
                transitions,
            },
            true,
            change,
        );
        // The strip being left keeps its visuals for the usual grace, so
        // coming straight back finds its panes as they were instead of
        // restoring every one of them. The panes now on screen get their
        // slots regardless: a visible card takes one from the hidden card
        // seen longest ago when the budget is full.
        let residency = self.lifecycle_sweep();
        let source_control = self.rebind_source_control();
        // Closed panes whose conversation stopped while this strip was not
        // shown go now that it is.
        let stopped_returns = self.remove_stopped_returns();
        Task::batch([viewport, residency, source_control, stopped_returns])
    }

    pub(super) fn terminal_directory(&self) -> Option<PathBuf> {
        if crate::device::is_remote() {
            return self
                .active_workspace
                .clone()
                .or_else(|| crate::device::home().cloned());
        }
        if let Some(path) = self
            .workspace
            .focused()
            .and_then(|card| self.terminal_working_directories.get(&card))
        {
            return Some(path.clone());
        }
        // A channel's directory rather than the strip it installed: the
        // channel's checkout may have been listed, or removed, since.
        if let Some(path) = self
            .durable
            .active_channel
            .and_then(|channel| self.channel_directory(channel))
            .or_else(|| self.active_workspace.clone())
        {
            return Some(path);
        }
        let card = self
            .workspace
            .focused()
            .and_then(|id| self.card_state(id))?;
        let directory = card_directory(card)?;
        Some(
            worktree_root(
                directory,
                self.sidebar_model
                    .projects
                    .iter()
                    .flat_map(|project| &project.worktrees)
                    .map(|worktree| worktree.path.as_path()),
            )
            .unwrap_or(directory)
            .to_path_buf(),
        )
    }

    /// Restore a store saved before windows kept their own view, as tests
    /// and workloads that seed only the durable state do.
    #[cfg(test)]
    pub(super) fn restore_worktree_scope(&mut self) -> Task<Message> {
        self.restore_workspace_scope(false, None)
    }

    /// The checkout a window saved on `removed` falls back to: one in the
    /// project its record names, otherwise in the nearest project that
    /// contains it. An unrelated project's worktree is never a substitute.
    fn surviving_project_scope(
        &self,
        removed: &Path,
        recorded: Option<ProjectId>,
    ) -> Option<PathBuf> {
        let available = |path: &Path| path != removed && worktree_directory_available(path);
        let checkout = |project: &SidebarProject| {
            project
                .worktrees
                .iter()
                .filter(|worktree| available(&worktree.path))
                .min_by_key(|worktree| {
                    !sidebar::is_primary_worktree(&project.repository, &worktree.path)
                })
                .map(|worktree| worktree.path.clone())
                .or_else(|| {
                    (project.kind == ProjectKind::Folder && available(&project.repository))
                        .then(|| project.repository.clone())
                })
        };
        // A worktree directory may live outside its repository, where only
        // the record ties it to the project.
        if let Some(target) = recorded
            .and_then(|id| self.sidebar_model.project(id))
            .and_then(checkout)
        {
            return Some(target);
        }
        self.sidebar_model
            .projects
            .iter()
            .filter_map(|project| {
                let root = project
                    .repository
                    .parent()
                    .filter(|parent| sidebar::is_primary_worktree(&project.repository, parent))
                    .unwrap_or(&project.repository);
                if !removed.starts_with(root)
                    && !project
                        .worktrees
                        .iter()
                        .any(|worktree| worktree.path == removed)
                {
                    return None;
                }
                Some((root.components().count(), checkout(project)?))
            })
            .max_by_key(|(depth, _)| *depth)
            .map(|(_, target)| target)
    }

    /// Must run before initialize_runtimes so hidden restored terminals are
    /// admitted as offscreen, not with another worktree's persisted exposure.
    ///
    /// `has_saved_view` says the store came from a build that records the
    /// window's scope, so no saved scope means the window was on home.
    pub(super) fn restore_workspace_scope(
        &mut self,
        has_saved_view: bool,
        scope_project: Option<ProjectId>,
    ) -> Task<Message> {
        Arc::make_mut(&mut self.durable).normalize_channels();
        // At startup these roots come from every catalog record, including
        // retired ones. Check that evidence before directory-bearing panes add
        // inferred roots: a live unregistered launch directory remains valid.
        let removed_scope = self.durable.active_worktree.clone().filter(|scope| {
            let catalogued = self.known_worktree_roots.contains(scope);
            (!worktree_directory_available(scope) && (has_saved_view || catalogued))
                || (catalogued && self.sidebar_model.worktree_at(scope).is_none())
        });
        if let Some(scope) = &removed_scope {
            // Retained panes keep their former boundary instead of falling
            // into the surviving parent project's strip.
            self.known_worktree_roots.insert(scope.clone());
        }
        self.refresh_worktree_owners(None);
        self.worktree_focus.clone_from(&self.durable.worktree_focus);
        if let Some(id) = self.durable.active_channel {
            if self
                .durable
                .channels
                .iter()
                .find(|channel| channel.id == id)
                .is_some_and(|channel| self.sidebar_model.project(channel.project_id).is_some())
            {
                return self.select_channel(id);
            }
            Arc::make_mut(&mut self.durable).active_channel = None;
        }
        if let Some(scope) = self.durable.active_worktree.clone() {
            if removed_scope.is_none() {
                return self.select_worktree(scope, false);
            }
            Arc::make_mut(&mut self.durable).active_worktree = None;
            if let Some(fallback) = self.surviving_project_scope(&scope, scope_project) {
                return self.select_worktree(fallback, false);
            }
        }
        let focused = self.durable.focused.and_then(|id| {
            self.durable
                .cards
                .iter()
                .find(|card| card.id == id && card.existence.is_open())
        });
        // A saved window view comes from a build that records active scopes:
        // their absence means home. Only older stores without a view need to
        // infer a scope from their focused pane's directory.
        let Some(owner) = focused.filter(|card| {
            !has_saved_view && removed_scope.is_none() && self.card_worktree(card).is_some()
        }) else {
            return self.restore_home(focused.is_some(), removed_scope.is_some());
        };
        let root = self
            .card_worktree(owner)
            .expect("selected card has a directory")
            .to_path_buf();
        // A global browser may retain focus in the restored scope, but never
        // becomes an ownership source for that scope.
        self.worktree_focus
            .insert(root.clone(), focused.map_or(owner.id, |card| card.id));
        self.select_worktree(root, false)
    }

    /// Shared panes are retained in every window's store even when the window
    /// has never entered their workspace. No selected scope and no focused
    /// pane is Workspace home, not a request to open the first retained
    /// terminal. Home may also hold global utilities or adopted terminals
    /// outside any known worktree; those stay only when this window had a
    /// pane focused.
    fn restore_home(&mut self, had_focus: bool, scope_removed: bool) -> Task<Message> {
        let mut home = self.durable.as_ref().clone();
        home.cards.retain(|card| {
            had_focus
                && card.existence.is_open()
                && !self.durable.card_scopes.contains_key(&card.id)
                && card_directory(card)
                    .is_none_or(|directory| self.worktree_scope_for(directory).is_none())
                && !self
                    .durable
                    .channels
                    .iter()
                    .any(|channel| channel.cards.contains(&card.id))
        });
        let width = self.workspace.viewport_width();
        self.workspace = Self::rebuild_workspace(&home);
        self.workspace.resize_viewport(width);
        self.rehide_closing_panes();
        if scope_removed {
            Arc::make_mut(&mut self.durable).focused = self.workspace.focused();
            self.persist();
        }
        let stopped_returns = self.remove_stopped_returns();
        Task::batch([stopped_returns, self.rebind_source_control()])
    }

    /// Where a new pane goes in the strip and in the saved cards: directly
    /// after the pane last clicked, typed in or focused by a command, or at
    /// the end when the strip has no such pane.
    ///
    /// A pane opened from another pane (a Source control row, a link, a
    /// pull request) opens beside it, since a press anywhere in a pane
    /// chooses it before its controls act. One opened from the sidebar or a
    /// menu opens beside the pane the person was working in, not one the
    /// pointer crossed on its way there; see [`Self::placement_anchor`].
    pub(super) fn pane_insertion_indices(&self) -> (usize, usize) {
        insertion_indices(
            self.workspace.cards(),
            &self.durable.cards,
            self.placement_anchor(),
        )
    }

    /// Where a pane opened from `opener` goes: directly after it, or as
    /// [`Self::pane_insertion_indices`] places it when `opener` is not in
    /// the strip.
    pub(super) fn pane_insertion_indices_after(&self, opener: CardId) -> (usize, usize) {
        if self.workspace.card(opener).is_some() {
            insertion_indices(self.workspace.cards(), &self.durable.cards, Some(opener))
        } else {
            self.pane_insertion_indices()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{BrowserId, BrowserRestoreFidelity, ProjectChannel};

    /// A submodule keeps its Git directory inside the superproject's, which
    /// is never where its panes should start: its main checkout is the row
    /// `git worktree list` names first. A Git directory named `.git` still
    /// finds its checkout beside it, wherever that row is listed.
    #[test]
    fn a_main_checkout_is_never_a_git_directory_kept_elsewhere() {
        let row = |path: &str| SidebarWorktree::from_durable(WorktreeId::new(), path.into(), None);
        let project = |repository: &str, rows: Vec<SidebarWorktree>| {
            SidebarProject::new(
                ProjectId::new(),
                RepositoryId::new(),
                "repo".into(),
                repository.into(),
                rows,
                Vec::new(),
            )
        };
        let submodule = project(
            "/super/.git/modules/sub",
            vec![row("/super/sub"), row("/super/sub-feature")],
        );
        assert_eq!(project_main_checkout(&submodule), Path::new("/super/sub"));
        let ordinary = project("/repo/.git", vec![row("/repo-feature"), row("/repo")]);
        assert_eq!(project_main_checkout(&ordinary), Path::new("/repo"));
    }

    thread_local! {
        /// How many cards this thread has searched every channel for.
        pub(super) static CHANNEL_SEARCHES: std::cell::Cell<usize> =
            const { std::cell::Cell::new(0) };
    }

    /// Showing a worktree asks of every pane whether it is in a channel and
    /// which worktree it belongs to. It answers from one set of channel
    /// members and one root index, never by searching every channel for
    /// each pane, which made a switch cost panes times channel members.
    #[test]
    fn showing_a_worktree_never_searches_the_channels_for_each_pane() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        register_worktrees(&mut app, &["/repo", "/repo/worktree"]);
        let project_id = app.sidebar_model.projects[0].id;
        let mut cards = Vec::new();
        for index in 0..48 {
            cards.push(terminal(if index % 2 == 0 {
                "/repo"
            } else {
                "/repo/worktree"
            }));
        }
        let channels = cards
            .chunks(8)
            .take(3)
            .map(|chunk| ProjectChannel {
                title_source: Default::default(),
                id: ChannelId::new(),
                project_id,
                title: "Channel".into(),
                ordinal: 0,
                shared: false,
                cards: chunk[..4].iter().map(|card| card.id).collect(),
                focused: None,
                checkout: None,
            })
            .collect::<Vec<_>>();
        let card_count = cards.len();
        app.durable = Arc::new(WorkspaceState {
            cards: cards.into(),
            active_worktree: Some("/repo".into()),
            channels,
            ..WorkspaceState::default()
        });
        let _ = app.restore_worktree_scope();

        CHANNEL_SEARCHES.set(0);
        // Each worktree has a terminal of its own, so none gains a default
        // pane: entering one, and showing the one already shown.
        let (shown, _) = app.select_worktree_checked("/repo/worktree".into(), true);
        assert!(shown);
        let (shown, _) = app.select_worktree_checked("/repo/worktree".into(), true);
        assert!(shown);
        let (shown, _) = app.navigate_to_worktree_checked("/repo".into(), true);
        assert!(shown);
        assert_eq!(CHANNEL_SEARCHES.get(), 0);
        assert_eq!(app.durable.cards.len(), card_count);
        assert_eq!(app.active_workspace.as_deref(), Some(Path::new("/repo")));
        // Of the twenty-four terminals in /repo, the channels hold six.
        assert_eq!(app.workspace.order().count(), 18);
    }

    #[test]
    fn selected_worktrees_and_channels_never_mix_unrelated_panes() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        register_worktrees(&mut app, &["/repo", "/repo/worktree"]);
        let project = terminal("/repo");
        let worktree = terminal("/repo/worktree");
        let home = terminal("/home/user");
        let loose = terminal("/tmp");
        let companion = terminal("/tmp");
        let channeled = terminal("/home/user");
        let channel = ChannelId::new();
        app.durable = Arc::new(WorkspaceState {
            cards: vec![
                project.clone(),
                worktree.clone(),
                home.clone(),
                loose.clone(),
                companion.clone(),
                channeled.clone(),
            ]
            .into(),
            active_worktree: Some("/repo".into()),
            channels: vec![ProjectChannel {
                title_source: Default::default(),
                id: channel,
                project_id: app.sidebar_model.projects[0].id,
                title: "Channel".into(),
                ordinal: 0,
                shared: false,
                cards: vec![channeled.id],
                focused: Some(channeled.id),
                checkout: None,
            }],
            ..WorkspaceState::default()
        });
        let _ = app.restore_worktree_scope();
        assert_eq!(app.workspace.order().collect::<Vec<_>>(), vec![project.id]);
        let _ = app.select_worktree("/repo/worktree".into(), false);
        assert_eq!(app.workspace.order().collect::<Vec<_>>(), vec![worktree.id]);
        let _ = app.select_channel(channel);
        assert_eq!(
            app.workspace.order().collect::<Vec<_>>(),
            vec![channeled.id]
        );
        let _ = app.select_worktree("/tmp".into(), false);
        assert_eq!(app.durable.active_channel, None);
        assert_eq!(
            app.workspace.order().collect::<Vec<_>>(),
            vec![loose.id, companion.id]
        );
        assert_eq!(app.terminal_directory(), Some(PathBuf::from("/tmp")));
        let _ = app.select_worktree("/home/user".into(), false);
        assert_eq!(app.workspace.order().collect::<Vec<_>>(), vec![home.id]);
        let _ = app.select_worktree("/repo".into(), false);
        assert_eq!(app.workspace.order().collect::<Vec<_>>(), vec![project.id]);
        assert_eq!(app.durable.cards.len(), 6);
    }
    #[test]
    fn same_directory_channels_restore_focus_without_entering_main_panes() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        register_worktrees(&mut app, &["/repo"]);
        app.sidebar_model.projects[0].repository = "/repo/.git".into();
        let project_id = app.sidebar_model.projects[0].id;
        let main = terminal("/repo");
        let first = terminal("/repo");
        let focused = terminal("/repo");
        let second = terminal("/repo");
        let a = ChannelId::new();
        let b = ChannelId::new();
        app.durable = Arc::new(WorkspaceState {
            cards: vec![main.clone(), first.clone(), focused.clone(), second.clone()].into(),
            focused: Some(focused.id),
            active_worktree: Some("/repo".into()),
            active_channel: Some(a),
            channels: vec![
                ProjectChannel {
                    title_source: Default::default(),
                    id: a,
                    project_id,
                    title: "A".into(),
                    ordinal: 0,
                    shared: false,
                    cards: vec![first.id, focused.id],
                    focused: Some(focused.id),
                    checkout: None,
                },
                ProjectChannel {
                    title_source: Default::default(),
                    id: b,
                    project_id,
                    title: "B".into(),
                    ordinal: 0,
                    shared: false,
                    cards: vec![second.id],
                    focused: Some(second.id),
                    checkout: None,
                },
            ],
            ..WorkspaceState::default()
        });
        let _ = app.restore_worktree_scope();
        assert_eq!(
            app.workspace.order().collect::<Vec<_>>(),
            vec![first.id, focused.id]
        );
        assert_eq!(app.workspace.focused(), Some(focused.id));
        assert_eq!(app.terminal_directory(), Some(PathBuf::from("/repo")));
        let _ = app.select_worktree("/repo".into(), false);
        assert_eq!(app.durable.active_channel, None);
        assert_eq!(app.workspace.order().collect::<Vec<_>>(), vec![main.id]);
        let _ = app.select_channel(b);
        assert_eq!(app.workspace.order().collect::<Vec<_>>(), vec![second.id]);
        let _ = app.select_channel(a);
        assert_eq!(
            app.workspace.order().collect::<Vec<_>>(),
            vec![first.id, focused.id]
        );
        assert_eq!(app.workspace.focused(), Some(focused.id));
        assert_eq!(
            app.durable
                .cards
                .iter()
                .map(|card| card.id)
                .collect::<Vec<_>>(),
            vec![main.id, first.id, focused.id, second.id]
        );
    }

    #[test]
    fn closing_last_channel_card_keeps_empty_scope_available_for_new_cards() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        register_worktrees(&mut app, &["/repo"]);
        let project_id = app.sidebar_model.projects[0].id;
        let id = ChannelId::new();
        let closed = terminal("/repo");
        let main = terminal("/repo");
        app.durable = Arc::new(WorkspaceState {
            cards: vec![main.clone(), closed.clone()].into(),
            channels: vec![ProjectChannel {
                title_source: Default::default(),
                id,
                project_id,
                title: "Channel".into(),
                ordinal: 0,
                shared: false,
                cards: vec![closed.id],
                focused: Some(closed.id),
                checkout: None,
            }],
            ..WorkspaceState::default()
        });
        let _ = app.select_channel(id);
        Arc::make_mut(&mut app.durable).cards[1].existence = Existence::Closing {
            operation_id: crate::model::OperationId::new(),
        };
        let _ = app.select_channel(id);
        app.snapshot_workspace_scope();
        assert_eq!(app.durable.active_channel, Some(id));
        assert_eq!(
            app.workspace.order().collect::<Vec<_>>(),
            Vec::<CardId>::new()
        );
        assert_eq!(app.durable.channels[0].focused, None);
        assert!(app.durable.channels[0].cards.is_empty());
        let replacement = terminal("/repo");
        Arc::make_mut(&mut app.durable)
            .cards
            .push(replacement.clone());
        app.register_channel_card(replacement.id);
        let _ = app.select_worktree("/repo".into(), false);
        assert_eq!(app.workspace.order().collect::<Vec<_>>(), vec![main.id]);
        let _ = app.select_channel(id);
        assert_eq!(
            app.workspace.order().collect::<Vec<_>>(),
            vec![replacement.id]
        );
        assert_eq!(app.workspace.focused(), Some(replacement.id));
    }

    fn repository(path: &str) -> CardState {
        CardState {
            id: CardId::new(),
            generation: 0,
            title: path.into(),
            width_fraction: 0.7,
            existence: Existence::Open,
            kind: CardKind::Repository(RepositoryView {
                selected_file: None,
                project: ProjectId::new(),
                repository: path.into(),
                worktree: path.into(),
            }),
        }
    }

    fn terminal(path: &str) -> CardState {
        let mut card = repository(path);
        let launch = LaunchRecipe {
            unset_environment: Default::default(),
            program: "/bin/sh".into(),
            arguments: Vec::new(),
            working_directory: path.into(),
            environment: Default::default(),
        };
        card.kind = CardKind::Terminal(TerminalSession {
            owned_codex_listener: None,
            recovery_origin: None,
            predecessor: None,
            id: TerminalId::new(),
            launch: launch.clone(),
            transcript_path: None,
            resume_hint: None,
            continuity: TerminalContinuity::RelaunchAvailable {
                recipe: launch,
                last_exit: None,
            },
        });
        card
    }

    /// The sidebar mutes a worktree row exactly while nothing is open there,
    /// so the derivation has to follow panes as they open and close, count a
    /// pane launched deeper inside the worktree, and leave channel cards to
    /// nobody's worktree.
    #[test]
    fn worktree_rows_know_whether_they_hold_an_open_pane() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        register_worktrees(&mut app, &["/repo", "/repo/worktree", "/repo/idle"]);
        let project_id = app.sidebar_model.projects[0].id;
        let row = |app: &Cagents, path: &str| {
            app.sidebar_model.projects[0]
                .worktrees
                .iter()
                .find(|worktree| worktree.path == Path::new(path))
                .expect("registered worktree")
                .id
        };
        let has_panes = |app: &Cagents, path: &str| {
            app.sidebar_state
                .worktree_has_panes(project_id, row(app, path))
        };
        let root = repository("/repo");
        let nested = terminal("/repo/worktree/src");
        let channeled = terminal("/repo/idle");
        app.durable = Arc::new(WorkspaceState {
            cards: vec![root.clone(), nested.clone(), channeled.clone()].into(),
            channels: vec![ProjectChannel {
                title_source: Default::default(),
                id: ChannelId::new(),
                project_id,
                title: "Channel".into(),
                ordinal: 0,
                shared: false,
                cards: vec![channeled.id],
                focused: Some(channeled.id),
                checkout: None,
            }],
            ..WorkspaceState::default()
        });

        app.refresh_sidebar_worktree_panes();
        assert!(has_panes(&app, "/repo"));
        assert!(has_panes(&app, "/repo/worktree"), "a nested launch counts");
        assert!(
            !has_panes(&app, "/repo/idle"),
            "a channel's card belongs to no worktree"
        );

        let durable = Arc::make_mut(&mut app.durable);
        durable.cards[1].existence = super::super::project_ui::removed_existence("closed");
        durable.cards[0].existence = Existence::Closing {
            operation_id: crate::model::OperationId::new(),
        };
        app.refresh_sidebar_worktree_panes();
        assert!(!has_panes(&app, "/repo"), "a closing pane is already gone");
        assert!(!has_panes(&app, "/repo/worktree"));

        Arc::make_mut(&mut app.durable)
            .cards
            .push(terminal("/repo/idle"));
        app.refresh_sidebar_worktree_panes();
        assert!(has_panes(&app, "/repo/idle"), "a newly opened pane counts");
    }

    /// The derivation is only as live as the update that runs it: closing a
    /// pane through the ordinary message path has to mute its row.
    #[test]
    fn closing_the_last_pane_through_update_mutes_its_worktree() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        register_worktrees(&mut app, &["/repo"]);
        let project_id = app.sidebar_model.projects[0].id;
        let worktree = app.sidebar_model.projects[0].worktrees[0].id;
        let shell = terminal("/repo");
        let id = shell.id;
        app.durable = Arc::new(WorkspaceState {
            cards: vec![shell].into(),
            ..WorkspaceState::default()
        });
        // Any message that reaches the end of `update` derives the rows.
        let _ = app.update(Message::CancelTerminalClose);
        assert!(app.sidebar_state.worktree_has_panes(project_id, worktree));

        app.workspace
            .add_card(
                WorkspaceCard::new(id, "Shell", CardWidth::Fraction(0.5)).unwrap(),
                0,
                true,
            )
            .unwrap();
        let _ = app.update(Message::CloseCard(id));
        let _ = app.update(Message::CommitClose(id));
        assert!(
            !app.durable
                .cards
                .iter()
                .any(|card| card.id == id && card.existence.is_open())
        );
        assert!(!app.sidebar_state.worktree_has_panes(project_id, worktree));
    }

    #[test]
    fn the_settings_pane_follows_the_user_between_worktrees_and_channels() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        register_worktrees(&mut app, &["/repo", "/repo/worktree"]);
        let main = terminal("/repo");
        let nested = terminal("/repo/worktree");
        let channeled = terminal("/home/user");
        let channel = ChannelId::new();
        app.durable = Arc::new(WorkspaceState {
            cards: vec![main.clone(), nested.clone(), channeled.clone()].into(),
            active_worktree: Some("/repo".into()),
            channels: vec![ProjectChannel {
                title_source: Default::default(),
                id: channel,
                project_id: app.sidebar_model.projects[0].id,
                title: "Channel".into(),
                ordinal: 0,
                shared: false,
                cards: vec![channeled.id],
                focused: Some(channeled.id),
                checkout: None,
            }],
            ..WorkspaceState::default()
        });
        let _ = app.restore_worktree_scope();
        let order = |app: &Cagents| app.workspace.order().collect::<Vec<_>>();
        let in_channel =
            |app: &Cagents, card: CardId| app.durable.channels[0].cards.contains(&card);

        // Opened from a worktree, it is nobody's worktree, like a browser.
        let _ = app.update(Message::Sidebar(SidebarAction::OpenSettings));
        let pane = app.utility_card(&CardKind::Settings).unwrap();
        assert_eq!(order(&app), [main.id, pane]);
        // It was filed beside the pane focused when it opened, which puts it
        // ahead of the nested worktree's panes in the shared order.
        let _ = app.select_worktree("/repo/worktree".into(), false);
        assert_eq!(order(&app), [pane, nested.id]);

        // A channel shows only its own cards, so asking for settings there
        // brings the one pane in rather than leaving the channel.
        let _ = app.select_channel(channel);
        assert_eq!(order(&app), [channeled.id]);
        let _ = app.update(Message::Sidebar(SidebarAction::OpenSettings));
        assert_eq!(app.durable.active_channel, Some(channel));
        assert_eq!(order(&app), [channeled.id, pane]);
        assert_eq!(app.workspace.focused(), Some(pane));
        assert!(in_channel(&app, pane));
        let _ = app.select_worktree("/repo".into(), false);
        assert_eq!(order(&app), [main.id]);

        // And back out again, still the same single pane.
        let _ = app.update(Message::Sidebar(SidebarAction::OpenSettings));
        assert_eq!(order(&app), [main.id, pane]);
        assert!(!in_channel(&app, pane));
        assert_eq!(
            app.durable
                .cards
                .iter()
                .filter(|card| card.kind == CardKind::Settings)
                .count(),
            1
        );
        let _ = app.select_channel(channel);
        assert_eq!(order(&app), [channeled.id]);
        assert_eq!(
            app.durable.channels[0].focused,
            Some(channeled.id),
            "the channel keeps a focus it still holds"
        );

        // Opened in a channel holding nothing else and closed again, it
        // leaves the channel as it found it rather than deleting it.
        let empty = ChannelId::new();
        Arc::make_mut(&mut app.durable)
            .channels
            .push(ProjectChannel {
                title_source: Default::default(),
                id: empty,
                project_id: app.sidebar_model.projects[0].id,
                title: "Empty".into(),
                ordinal: 1,
                shared: true,
                cards: Vec::new(),
                focused: None,
                checkout: None,
            });
        let _ = app.select_channel(empty);
        let _ = app.update(Message::Sidebar(SidebarAction::OpenSettings));
        assert_eq!(order(&app), [pane]);
        let _ = app.request_card_removal(pane);
        let _ = app.update(Message::CommitClose(pane));
        assert!(app.utility_card(&CardKind::Settings).is_none());
        assert!(
            app.durable
                .channels
                .iter()
                .any(|channel| channel.id == empty)
        );
        assert_eq!(app.durable.active_channel, Some(empty));

        // Visiting a channel that is deleted while the user is elsewhere, it
        // returns to the worktree strip on screen at once.
        let _ = app.update(Message::Sidebar(SidebarAction::OpenSettings));
        let pane = app.utility_card(&CardKind::Settings).unwrap();
        let _ = app.select_worktree("/repo".into(), false);
        assert_eq!(order(&app), [main.id]);
        let _ = app.handle_channel_ui(super::super::channel_ui::Action::Delete(empty));
        assert!(
            !app.durable
                .channels
                .iter()
                .any(|channel| channel.id == empty)
        );
        assert!(order(&app).contains(&pane));
    }

    fn register_worktrees(app: &mut Cagents, paths: &[&str]) {
        *app.sidebar_model.projects = vec![SidebarProject::new(
            ProjectId::new(),
            RepositoryId::new(),
            "repo".into(),
            paths[0].into(),
            paths
                .iter()
                .map(|path| {
                    SidebarWorktree::new(
                        crate::model::WorktreeId::new(),
                        (*path).into(),
                        None,
                        false,
                        false,
                        None,
                        None,
                        None,
                    )
                })
                .collect(),
            Vec::new(),
        )];
    }

    fn acknowledge_empty_default_scope(app: &mut Cagents) {
        let directory = app.active_workspace.clone().unwrap();
        let scope = app.default_pane_scope(&directory).unwrap();
        if app.workspace_model.identity == crate::workspace_model::NO_REGISTRY {
            app.workspace_model.identity = uuid::Uuid::new_v4();
        }
        app.workspace_model.views = vec![crate::workspace_model::views::View::empty(scope.clone())];
        app.acknowledge_view_edit(scope, None);
        drop(app.reconcile_shared_views());
    }

    #[test]
    fn default_pane_is_recreated_when_its_empty_scope_is_restored() {
        for channel in [false, true] {
            let mut app = super::super::tests::context_inspector_test_app(false);
            register_worktrees(&mut app, &["/repo"]);
            if channel {
                let project = app.sidebar_model.projects[0].id;
                let _ = app.handle_channel_ui(channel_ui::Action::Create(project));
            } else {
                let _ = app.select_worktree("/repo".into(), true);
                acknowledge_empty_default_scope(&mut app);
            }
            let previous = app.workspace.focused().unwrap();
            let saved = app.persistent_workspace();
            assert!(!saved.cards.iter().any(|card| card.id == previous));
            assert!(
                !saved
                    .channels
                    .iter()
                    .any(|channel| channel.cards.contains(&previous))
            );
            let mut restored = super::super::tests::context_inspector_test_app(false);
            restored.sidebar_model = app.sidebar_model.clone();
            restored.durable = saved;
            let _ = restored.restore_worktree_scope();
            let _ = restored.initialize_runtimes();
            let _ = restored.ensure_default_pane();
            assert!(restored.workspace.cards().is_empty());
            acknowledge_empty_default_scope(&mut restored);
            let fresh = restored.workspace.focused().unwrap();
            assert_ne!(fresh, previous);
            assert!(matches!(
                restored.card_state(fresh).unwrap().kind,
                CardKind::Chat(_)
            ));
            assert_eq!(restored.durable.active_channel.is_some(), channel);
        }
    }

    #[test]
    fn default_pane_survives_discovery_promoting_its_directory() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        let _ = app.select_worktree("/repo/src".into(), true);
        let card = app.workspace.focused().unwrap();
        register_worktrees(&mut app, &["/repo"]);
        let _ = app.select_worktree("/repo".into(), true);
        assert_eq!(app.workspace.focused(), Some(card));
        assert!(app.card_state(card).unwrap().existence.is_open());
    }

    #[test]
    fn default_panes_for_channels_and_worktrees_are_independent() {
        for (worktree, channel) in [
            (
                settings::DefaultPane::NativeChat,
                settings::DefaultPane::Terminal,
            ),
            (
                settings::DefaultPane::Terminal,
                settings::DefaultPane::NativeChat,
            ),
        ] {
            let mut app = super::super::tests::context_inspector_test_app(false);
            // These selection checks run after the shared registry has hydrated.
            app.workspace_model.identity = uuid::Uuid::new_v4();
            register_worktrees(&mut app, &["/repo"]);
            app.settings.default_worktree_pane = worktree;
            app.settings.default_channel_pane = channel;
            let project = app.sidebar_model.projects[0].id;
            let _ = app.handle_channel_ui(channel_ui::Action::Create(project));
            let channel_id = app.durable.active_channel.unwrap();
            let channel_card = app.workspace.focused().unwrap();
            app.shared_views.initialize(
                crate::workspace_model::views::Scope::Channel(channel_id),
                Vec::new(),
            );
            assert_eq!(
                matches!(
                    app.card_state(channel_card).unwrap().kind,
                    CardKind::Chat(_)
                ),
                channel == settings::DefaultPane::NativeChat
            );
            let _ = app.select_worktree("/repo".into(), true);
            acknowledge_empty_default_scope(&mut app);
            let worktree_card = app.workspace.focused().unwrap();
            assert_ne!(worktree_card, channel_card);
            assert_eq!(
                matches!(
                    app.card_state(worktree_card).unwrap().kind,
                    CardKind::Chat(_)
                ),
                worktree == settings::DefaultPane::NativeChat
            );
            let _ = app.handle_channel_ui(channel_ui::Action::Open(channel_id));
            let reopened = app.workspace.focused().unwrap();
            assert_eq!(
                matches!(app.card_state(reopened).unwrap().kind, CardKind::Chat(_)),
                channel == settings::DefaultPane::NativeChat
            );
            if channel == settings::DefaultPane::NativeChat {
                assert!(app.card_state(channel_card).is_none());
                assert_ne!(reopened, channel_card);
            } else {
                assert_eq!(reopened, channel_card);
            }
            assert_eq!(
                app.durable.channels.len(),
                1,
                "leaving an unused pane keeps its channel"
            );
        }
    }

    #[test]
    fn chat_owns_nested_scope_and_prevents_duplicate_terminal_creation() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        register_worktrees(&mut app, &["/repo", "/repo/nested", "/other"]);
        let mut card = terminal("/repo/nested/src");
        let CardKind::Terminal(source) = card.kind else {
            unreachable!()
        };
        card.kind = CardKind::Chat(crate::chat::Session {
            native_continuity: Default::default(),
            owned_codex_listener: None,
            id: source.id,
            launch: crate::chat::Launch {
                provider: crate::agent::Provider::Omp,
                recipe: source.launch.clone(),
                session_id: None,
                draft_model: None,
            },
            proof: None,
            terminal: Some(Box::new(source)),
        });
        app.durable = Arc::new(WorkspaceState {
            cards: vec![card.clone()].into(),
            ..WorkspaceState::default()
        });
        let _ = app.select_worktree("/repo/nested".into(), true);
        assert_eq!(app.durable.cards.len(), 1);
        assert_eq!(app.workspace.focused(), Some(card.id));
        assert!(app.workspace.order().any(|id| id == card.id));
        let _ = app.select_worktree("/repo".into(), false);
        assert!(!app.workspace.order().any(|id| id == card.id));
        let _ = app.select_worktree("/repo/nested".into(), true);
        assert_eq!(app.durable.cards.len(), 1);
        assert_eq!(app.workspace.focused(), Some(card.id));
    }

    #[test]
    fn switched_strip_accepts_scroll_operations_after_native_tree_diff() {
        use cosmic::iced::advanced::widget::{Operation, Tree};
        use cosmic::iced::advanced::{Layout, layout, renderer::Headless, widget::operation};
        use cosmic::iced::{Rectangle, Size, Vector};
        let renderer = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(cosmic::Renderer::new(
                cosmic::iced::Font::default(),
                cosmic::iced::Pixels(14.0),
                Some("tiny-skia"),
            ))
            .unwrap();
        let mut app = super::super::tests::context_inspector_test_app(false);
        register_worktrees(&mut app, &["/a", "/b"]);
        let a = terminal("/a");
        let b = terminal("/b");
        app.durable = Arc::new(WorkspaceState {
            cards: vec![a, b].into(),
            ..WorkspaceState::default()
        });
        let mut workspace = Workspace::default();
        workspace.resize_viewport(300.0);
        for index in 0..3 {
            workspace
                .add_card(
                    WorkspaceCard::new(
                        CardId::new(),
                        format!("Pane {index}"),
                        crate::workspace::CardWidth::Fixed(500.0),
                    )
                    .unwrap(),
                    index,
                    false,
                )
                .unwrap();
        }
        let shortcuts = app.settings.keyboard_shortcuts.clone();
        let make = |id| -> Element<'_, ()> {
            workspace_widget::view(
                &workspace,
                &shortcuts,
                workspace_widget::FocusedPane::Other,
                true,
                &crate::input_trace::InputTrace::default(),
                300.0,
                id,
                |_| {
                    iced::widget::Space::new()
                        .width(Length::Fill)
                        .height(Length::Fill)
                        .into()
                },
                workspace_widget::card_widget_id,
                workspace_widget::Events {
                    viewport: |_| (),
                    reads_on_scroll: |_| false,
                },
            )
        };
        let _ = app.select_worktree("/a".into(), false);
        let mut element = make(app.workspace_scroll_id.clone());
        let mut tree = Tree::new(&element);
        let limits = layout::Limits::new(Size::ZERO, Size::new(300.0, 100.0));
        element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        let _ = app.select_worktree("/b".into(), false);
        cosmic::iced::advanced::widget::tree::NAMED
            .with(|cache| *cache.borrow_mut() = tree.take_all_named());
        element = make(app.workspace_scroll_id.clone());
        tree.diff(&mut element);
        let node = element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        element.as_widget_mut().operate(
            &mut tree,
            Layout::new(&node),
            &renderer,
            &mut operation::scrollable::scroll_to::<()>(
                app.workspace_scroll_id.clone(),
                operation::scrollable::AbsoluteOffset {
                    x: Some(240.0),
                    y: None,
                },
            ),
        );
        #[derive(Default)]
        struct Offset(f32);
        impl Operation for Offset {
            fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                visit(self);
            }
            fn scrollable(
                &mut self,
                _: Option<&widget::Id>,
                _: Rectangle,
                _: Rectangle,
                translation: Vector,
                _: &mut dyn operation::Scrollable,
            ) {
                self.0 = translation.x;
            }
        }
        let mut offset = Offset::default();
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut offset);
        assert_eq!(offset.0, 240.0);
        cosmic::iced::advanced::widget::tree::NAMED.with(|cache| cache.borrow_mut().clear());
    }

    #[test]
    fn switching_strips_restores_exact_scroll_and_ignores_stale_native_feedback() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        register_worktrees(&mut app, &["/a", "/b"]);
        let a = [terminal("/a"), terminal("/a/src"), terminal("/a")];
        let b = terminal("/b");
        app.durable = Arc::new(WorkspaceState {
            cards: vec![a[0].clone(), b.clone(), a[1].clone(), a[2].clone()].into(),
            focused: Some(a[2].id),
            ..WorkspaceState::default()
        });
        app.workspace.resize_viewport(1_000.0);
        let _ = app.restore_worktree_scope();
        app.workspace.native_scroll(237.5, 1_000.0);
        let a_generation = app.workspace_view_generation;

        let _ = app.select_worktree("/b".into(), true);
        let b_generation = app.workspace_view_generation;
        assert_eq!(app.workspace.order().collect::<Vec<_>>(), vec![b.id]);
        assert_eq!(app.workspace.camera().offset(), 0.0);
        assert!(
            a.iter()
                .all(|card| app.exposure(card.id) == Exposure::Offscreen)
        );
        let _ = app.update(Message::Viewport {
            generation: a_generation,
            event: WorkspaceViewportEvent::Resized(400.0),
        });
        assert_eq!(app.workspace.viewport_width(), 1_000.0);

        let _ = app.select_worktree("/a".into(), true);
        assert_eq!(
            app.workspace.order().collect::<Vec<_>>(),
            a.iter().map(|card| card.id).collect::<Vec<_>>()
        );
        assert_eq!(app.workspace.focused(), Some(a[2].id));
        assert_eq!(app.workspace.camera().offset(), 237.5);
        for generation in [a_generation, b_generation] {
            let _ = app.update(Message::Viewport {
                generation,
                event: WorkspaceViewportEvent::Scrolled(workspace_widget::ViewportSample {
                    offset: 0.0,
                    width: 1_000.0,
                    user_input: true,
                }),
            });
        }
        assert_eq!(app.workspace.camera().offset(), 237.5);
        let _ = app.select_worktree("/a/src".into(), true);
        assert_eq!(app.workspace.camera().offset(), 237.5);
        assert_eq!(app.workspace.focused(), Some(a[2].id));
        assert_eq!(app.durable.cards.len(), 4);
        assert!(
            app.durable
                .cards
                .iter()
                .all(|card| card.existence.is_open())
        );

        let generation = app.workspace_view_generation;
        let _ = app.update(Message::Viewport {
            generation,
            event: WorkspaceViewportEvent::Scrolled(workspace_widget::ViewportSample {
                offset: 319.0,
                width: 1_000.0,
                user_input: true,
            }),
        });
        assert_eq!(app.workspace.camera().offset(), 319.0);
    }

    #[test]
    fn registering_legacy_directories_rescopes_the_current_strip_without_losing_position() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        let a = [terminal("/a/src"), terminal("/a/src"), terminal("/a/src")];
        let b = terminal("/b");
        app.durable = Arc::new(WorkspaceState {
            cards: vec![a[0].clone(), a[1].clone(), a[2].clone(), b.clone()].into(),
            focused: Some(a[0].id),
            ..WorkspaceState::default()
        });
        app.workspace.resize_viewport(1_000.0);
        let _ = app.restore_worktree_scope();
        app.workspace.native_scroll(250.0, 1_000.0);
        assert!(app.workspace.card(b.id).is_none());

        register_worktrees(&mut app, &["/a", "/b"]);
        let _ = app.select_worktree("/a".into(), false);
        assert!(app.workspace.card(b.id).is_none());
        assert_eq!(app.exposure(b.id), Exposure::Offscreen);
        assert_eq!(app.workspace.camera().offset(), 250.0);
        assert_eq!(app.workspace.focused(), Some(a[0].id));
    }

    /// A row clicked in the first of five panes' inspector opens second,
    /// not in the browser at the end of the strip, and the rows clicked
    /// after it reuse that browser rather than opening more.
    #[test]
    fn a_file_opened_from_a_panes_inspector_opens_directly_after_it() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        register_worktrees(&mut app, &["/a"]);
        let shells = [
            terminal("/a"),
            terminal("/a"),
            terminal("/a"),
            terminal("/a"),
        ];
        let far = repository("/a");
        let ids = shells
            .iter()
            .chain([&far])
            .map(|card| card.id)
            .collect::<Vec<_>>();
        app.durable = Arc::new(WorkspaceState {
            cards: shells
                .iter()
                .chain([&far])
                .cloned()
                .collect::<Vec<_>>()
                .into(),
            focused: Some(ids[3]),
            ..WorkspaceState::default()
        });
        app.window_width = 1_600.0;
        app.context_sections
            .set_open(context_sections::Section::Workspace, true);
        let _ = app.restore_worktree_scope();
        drop(app.workspace.resize_viewport(2_400.0));
        app.open_pane_inspector_now(ids[0]);
        drop(app.update(Message::FocusCard(ids[0])));
        assert!(app.pane_shows_source_control(ids[0]));
        let key = app.sidebar_state.active_worktree().unwrap().clone();
        let open = |app: &mut Cagents, path: &str| {
            drop(app.update(Message::Sidebar(SidebarAction::OpenFile {
                key: key.clone(),
                path: RepoPath::parse(path).unwrap(),
            })));
        };

        open(&mut app, "src/main.rs");
        let opened = app.workspace.focused().unwrap();
        assert!(!ids.contains(&opened), "the far browser is left alone");
        let order = [ids[0], opened, ids[1], ids[2], ids[3], ids[4]];
        assert_eq!(app.workspace.order().collect::<Vec<_>>(), order);
        assert_eq!(app.durable.cards[1].id, opened);

        // The next row, still clicked in pane 1's inspector, replaces it.
        open(&mut app, "src/lib.rs");
        assert_eq!(app.workspace.focused(), Some(opened));
        assert_eq!(app.workspace.order().collect::<Vec<_>>(), order);
        assert_eq!(
            app.pending_panel_open.as_ref().map(|open| open.card),
            Some(opened)
        );
    }

    #[test]
    fn file_selection_opens_beside_its_pane_and_leaves_the_inspector_showing_changes() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        register_worktrees(&mut app, &["/a", "/b"]);
        let editor = repository("/a");
        let shell = terminal("/a");
        app.durable = Arc::new(WorkspaceState {
            cards: vec![editor.clone(), shell.clone()].into(),
            focused: Some(shell.id),
            ..WorkspaceState::default()
        });
        app.window_width = 1_600.0;
        app.context_sections
            .set_open(context_sections::Section::Workspace, true);
        let _ = app.restore_worktree_scope();
        drop(app.workspace.resize_viewport(2_400.0));
        app.open_pane_inspector_now(shell.id);
        let key = app.sidebar_state.active_worktree().unwrap().clone();
        let _ = app.route_panel_open(
            &key,
            &RepoPath::parse("src/main.rs").unwrap(),
            app.panel_opener(),
        );
        // The browser before the shell is not the one after it, so the file
        // opens in a new browser beside the shell.
        let opened = app.workspace.focused().unwrap();
        assert_eq!(
            app.workspace.order().collect::<Vec<_>>(),
            [editor.id, shell.id, opened]
        );
        // The shell's inspector stays open, still drawing the changes the
        // file was chosen from, beside the editor it opened in.
        assert!(app.pane_inspectors.is_open(shell.id));
        assert!(app.pane_shows_source_control(shell.id));
        assert_eq!(
            app.workspace_view_shown(),
            Some(context_sections::WorkspaceView::SourceControl)
        );
        assert!(!app.workspace_inputs_blocked());

        let _ = app.select_worktree("/b".into(), false);
        app.pending_panel_open = None;
        let _ = app.route_panel_open(
            &key,
            &RepoPath::parse("src/main.rs").unwrap(),
            app.panel_opener(),
        );
        assert!(app.pending_panel_open.is_none());
        assert!(app.workspace.cards().is_empty());
    }

    #[test]
    fn legacy_unscoped_terminals_remain_accessible_without_owning_new_worktrees() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        // A connected publisher keeps the new scope's seed pending until the
        // registry answers, as a running window's does.
        let (tx, _requests) = super::super::workspace_model_sync::test_model_channel();
        app.workspace_model_tx = tx;
        // These selection checks run after the shared registry has hydrated.
        app.workspace_model.identity = uuid::Uuid::new_v4();
        app.settings.default_worktree_pane = settings::DefaultPane::Terminal;
        let legacy = terminal("/home/user");
        let owned = terminal("/repo");
        app.durable = Arc::new(WorkspaceState {
            cards: vec![legacy.clone(), owned.clone()].into(),
            focused: Some(owned.id),
            ..WorkspaceState::default()
        });
        register_worktrees(&mut app, &["/repo", "/other"]);

        let _ = app.select_worktree("/repo".into(), false);
        assert!(app.workspace.card(legacy.id).is_none());
        let _ = app.select_worktree("/other".into(), true);
        // The default terminal waits until the new scope's shared panes load.
        assert!(app.workspace.cards().is_empty());
        acknowledge_empty_default_scope(&mut app);
        assert!(app.workspace.card(legacy.id).is_none());
        assert!(app.workspace.card(owned.id).is_none());
        let focused = app.card_state(app.workspace.focused().unwrap()).unwrap();
        assert!(matches!(&focused.kind, CardKind::Terminal(session)
            if session.launch.working_directory == Path::new("/other")));
        assert_ne!(focused.id, legacy.id);
        assert_eq!(app.durable.cards.len(), 3);
    }

    #[cfg(unix)]
    #[test]
    fn canonical_launch_aliases_use_the_nested_owner_without_changing_the_recipe() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("repo");
        let nested = root.join("worktrees/feature");
        std::fs::create_dir_all(nested.join("src")).unwrap();
        let alias = directory.path().join("alias");
        std::os::unix::fs::symlink(&root, &alias).unwrap();
        let shell = terminal(alias.join("worktrees/feature/src").to_str().unwrap());
        let editor = repository(alias.to_str().unwrap());
        let mut app = super::super::tests::context_inspector_test_app(false);
        register_worktrees(
            &mut app,
            &[root.to_str().unwrap(), nested.to_str().unwrap()],
        );
        app.durable = Arc::new(WorkspaceState {
            cards: vec![shell.clone(), editor.clone()].into(),
            ..WorkspaceState::default()
        });
        let _ = app.select_worktree(alias.join("worktrees/feature"), false);
        assert_eq!(app.active_workspace.as_deref(), Some(nested.as_path()));
        assert_eq!(app.workspace.order().collect::<Vec<_>>(), vec![shell.id]);
        assert_eq!(app.card_state(shell.id).unwrap().kind, shell.kind);
        let _ = app.select_worktree(alias, false);
        assert_eq!(app.workspace.order().collect::<Vec<_>>(), vec![editor.id]);
    }

    /// A root is resolved once, when it becomes known. One that did not
    /// exist yet is tried again only once Projects lists a row there, as it
    /// does for a checkout created later; a deleted one is not tried again.
    #[cfg(unix)]
    #[test]
    fn known_roots_resolve_once_and_again_when_a_row_lists_one() {
        let directory = tempfile::tempdir().unwrap();
        let real = directory.path().join("real");
        let alias = directory.path().join("alias");
        std::fs::create_dir_all(real.join("existing")).unwrap();
        std::os::unix::fs::symlink(&real, &alias).unwrap();
        let mut known = KnownWorktreeRoots::from_iter([
            alias.join("existing"),
            alias.join("later"),
            alias.join("deleted"),
        ]);
        let mut index = WorktreePathIndex::default();
        let mut projects = Vec::new();
        index.refresh(&known, &projects);
        known.resolve_new(&index);
        let roots = known.canonical.roots.clone();
        assert_eq!(roots[&real.join("existing")], [alias.join("existing")]);
        assert_eq!(roots[&alias.join("later")], [alias.join("later")]);
        assert_eq!(roots[&alias.join("deleted")], [alias.join("deleted")]);
        assert_eq!(known.canonical.resolutions, 3);

        // Nothing new: every form is reused, even once the directory exists.
        std::fs::create_dir_all(real.join("later")).unwrap();
        index.refresh(&known, &projects);
        known.resolve_new(&index);
        assert_eq!(known.canonical.resolutions, 3);

        // The checkout gains its row, so it is resolved; the deleted root,
        // still without one, is not tried again.
        projects.push(SidebarProject::new(
            ProjectId::new(),
            RepositoryId::new(),
            "repo".into(),
            alias.join("existing"),
            vec![SidebarWorktree::new(
                crate::model::WorktreeId::new(),
                alias.join("later"),
                None,
                false,
                false,
                None,
                None,
                None,
            )],
            Vec::new(),
        ));
        index.refresh(&known, &projects);
        known.resolve_new(&index);
        let roots = known.canonical.roots.clone();
        assert_eq!(known.canonical.resolutions, 4);
        assert_eq!(roots[&real.join("later")], [alias.join("later")]);
        assert!(!roots.contains_key(&alias.join("later")));
        assert_eq!(roots[&alias.join("deleted")], [alias.join("deleted")]);

        // A newly known root is resolved alone.
        std::fs::create_dir_all(real.join("new")).unwrap();
        assert!(known.insert(alias.join("new")));
        assert!(!known.insert(alias.join("new")));
        index.refresh(&known, &projects);
        known.resolve_new(&index);
        let roots = known.canonical.roots.clone();
        assert_eq!(known.canonical.resolutions, 5);
        assert_eq!(roots[&real.join("new")], [alias.join("new")]);

        // A row can list a checkout still being created. It is tried on each
        // pass until it exists.
        known.insert(alias.join("creating"));
        projects[0].worktrees.push(SidebarWorktree::new(
            crate::model::WorktreeId::new(),
            alias.join("creating"),
            None,
            false,
            false,
            None,
            None,
            None,
        ));
        index.refresh(&known, &projects);
        known.resolve_new(&index);
        assert_eq!(known.canonical.resolutions, 7, "known, then listed");
        known.resolve_new(&index);
        assert_eq!(known.canonical.resolutions, 8);
        std::fs::create_dir_all(real.join("creating")).unwrap();
        known.resolve_new(&index);
        assert_eq!(known.canonical.resolutions, 9);
        assert_eq!(
            known.canonical.roots[&real.join("creating")],
            [alias.join("creating")]
        );
        known.resolve_new(&index);
        assert_eq!(known.canonical.resolutions, 9);
    }

    /// A checkout deleted after it was resolved still owns the panes filed
    /// under the path it became known by, as it did while it existed.
    #[cfg(unix)]
    #[test]
    fn a_deleted_checkout_known_through_an_alias_keeps_its_panes() {
        let directory = tempfile::tempdir().unwrap();
        let real = directory.path().join("real");
        let alias = directory.path().join("alias");
        let root = alias.join("repo");
        let nested = root.join("worktrees/feature");
        std::fs::create_dir_all(real.join("repo/worktrees/feature/src")).unwrap();
        std::os::unix::fs::symlink(&real, &alias).unwrap();
        let shell = terminal(nested.join("src").to_str().unwrap());
        let mut app = super::super::tests::context_inspector_test_app(false);
        register_worktrees(
            &mut app,
            &[root.to_str().unwrap(), nested.to_str().unwrap()],
        );
        app.durable = Arc::new(WorkspaceState {
            cards: vec![shell.clone()].into(),
            ..WorkspaceState::default()
        });
        let _ = app.select_worktree(nested.clone(), false);
        assert_eq!(app.worktree_owners[&shell.id], nested);
        std::fs::remove_dir_all(real.join("repo/worktrees")).unwrap();
        app.refresh_worktree_owners(None);
        assert_eq!(app.worktree_owners[&shell.id], nested);
    }

    /// Switching worktrees resolves only the panes' directories and the
    /// selection, never the roots already known.
    #[cfg(unix)]
    #[test]
    fn switching_worktrees_reuses_the_known_roots_canonical_forms() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("repo");
        let nested = root.join("worktrees/feature");
        std::fs::create_dir_all(nested.join("src")).unwrap();
        let shell = terminal(nested.join("src").to_str().unwrap());
        let mut app = super::super::tests::context_inspector_test_app(false);
        register_worktrees(
            &mut app,
            &[root.to_str().unwrap(), nested.to_str().unwrap()],
        );
        app.durable = Arc::new(WorkspaceState {
            cards: vec![shell.clone()].into(),
            ..WorkspaceState::default()
        });
        let _ = app.select_worktree(nested.clone(), false);
        let resolved = app.known_worktree_roots.canonical.resolutions;
        assert!(resolved >= 2, "both rows were resolved once");
        let _ = app.select_worktree(root.clone(), false);
        let _ = app.select_worktree(nested.clone(), false);
        assert_eq!(app.known_worktree_roots.canonical.resolutions, resolved);
        assert_eq!(app.workspace.order().collect::<Vec<_>>(), vec![shell.id]);
    }

    #[test]
    fn missing_worktree_is_not_a_navigation_row_but_its_session_remains_reachable() {
        let directory = tempfile::tempdir().unwrap();
        let missing = directory.path().join("deleted-worktree");
        assert!(worktree_directory_available(directory.path()));
        assert!(!worktree_directory_available(&missing));
        let shell = terminal(missing.to_str().unwrap());
        let mut app = super::super::tests::context_inspector_test_app(false);
        app.durable = Arc::new(WorkspaceState {
            cards: vec![shell.clone()].into(),
            ..WorkspaceState::default()
        });
        let _ = app.select_worktree(directory.path().into(), false);
        assert!(app.workspace.cards().is_empty());
        assert!(app.card_state(shell.id).unwrap().existence.is_open());
    }

    fn restored(path: &Path, existence: Existence) -> WorktreeState {
        WorktreeState {
            id: crate::model::WorktreeId::new(),
            project_id: ProjectId::new(),
            generation: 3,
            path: path.into(),
            branch: None,
            existence,
        }
    }

    #[test]
    fn restored_records_of_missing_worktrees_are_retired_once() {
        let directory = tempfile::tempdir().unwrap();
        let mut worktrees = [
            restored(directory.path(), Existence::Open),
            restored(&directory.path().join("removed"), Existence::Open),
            restored(&directory.path().join("deleted"), removed_outside()),
        ];
        let retired = retire_missing_worktrees(&mut worktrees);
        assert_eq!(
            retired
                .iter()
                .map(|worktree| worktree.id)
                .collect::<Vec<_>>(),
            [worktrees[1].id]
        );
        assert!(worktrees[0].existence.is_open());
        assert!(!worktrees[1].existence.is_open());
        assert!(retire_missing_worktrees(&mut worktrees).is_empty());
    }

    #[test]
    fn deleted_nested_worktree_keeps_its_sessions_out_of_the_parent_scope() {
        for restored_without_row in [false, true] {
            let mut app = super::super::tests::context_inspector_test_app(false);
            let parent = terminal("/repo");
            let retained = terminal("/repo/worktrees/deleted/src");
            app.durable = Arc::new(WorkspaceState {
                cards: vec![parent.clone(), retained.clone()].into(),
                ..WorkspaceState::default()
            });
            if restored_without_row {
                // Startup seeds boundaries from durable worktree records before
                // removing missing rows from navigation.
                app.known_worktree_roots
                    .insert("/repo/worktrees/deleted".into());
            } else {
                register_worktrees(&mut app, &["/repo", "/repo/worktrees/deleted"]);
                let _ = app.select_worktree("/repo".into(), false);
            }
            register_worktrees(&mut app, &["/repo"]);
            let _ = app.select_worktree("/repo".into(), false);
            assert_eq!(app.workspace.order().collect::<Vec<_>>(), vec![parent.id]);
            assert_eq!(app.exposure(retained.id), Exposure::Offscreen);
            assert!(app.card_state(retained.id).unwrap().existence.is_open());
        }
    }

    #[test]
    fn cloned_and_reopened_nested_editors_keep_their_workspace_on_restart() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().to_path_buf();
        let nested = root.join("src");
        std::fs::create_dir(&nested).unwrap();
        let source = repository(nested.to_str().unwrap());
        let mut app = super::super::tests::context_inspector_test_app(false);
        app.durable = Arc::new(WorkspaceState {
            cards: vec![source.clone()].into(),
            focused: Some(source.id),
            active_worktree: Some(root.clone()),
            card_scopes: HashMap::from([(source.id, root.clone())]),
            ..WorkspaceState::default()
        });
        *app.sidebar_model.projects = vec![SidebarProject::new(
            ProjectId::new(),
            RepositoryId::new(),
            "repo".into(),
            root.clone(),
            vec![SidebarWorktree::new(
                crate::model::WorktreeId::new(),
                root.clone(),
                None,
                false,
                false,
                None,
                None,
                None,
            )],
            Vec::new(),
        )];
        app.workspace = Cagents::rebuild_workspace(&app.durable);
        app.workspace.resize_viewport(1_280.0);
        let _ = app.restore_worktree_scope();

        let _ = app.clone_focused_pane();
        let cloned = app.workspace.focused().unwrap();
        assert_ne!(cloned, source.id);
        assert_eq!(app.worktree_owners[&cloned], root);
        let _ = app.select_worktree(root.clone(), false);
        assert_eq!(
            app.workspace.order().collect::<Vec<_>>(),
            vec![source.id, cloned]
        );

        let _ = app.begin_card_close(cloned);
        assert_eq!(app.workspace.pending_close().unwrap().unmounting, cloned);
        // The native focus handoff publishes CommitClose after the reserved
        // pane yields focus. Drive that acknowledgement before reopening.
        let _ = app.update(Message::CommitClose(cloned));
        assert!(!app.card_state(cloned).unwrap().existence.is_open());
        assert!(!app.durable.card_scopes.contains_key(&cloned));
        let _ = app.run_workspace_command(
            crate::workspace_widget::GlobalShortcut::ReopenPane,
            cosmic::iced::keyboard::Modifiers::empty(),
        );
        let reopened = app.workspace.focused().unwrap();
        assert_ne!(reopened, cloned);
        assert_ne!(reopened, source.id);
        assert_eq!(app.worktree_owners[&reopened], root);
        assert_eq!(app.durable.card_scopes[&reopened], root);

        // Reconstruct from the serialized durable state, with empty ownership
        // caches, using the same startup projection as a fresh application.
        let saved = serde_json::to_string(app.durable.as_ref()).unwrap();
        let mut restarted = super::super::tests::context_inspector_test_app(false);
        restarted.durable = Arc::new(serde_json::from_str(&saved).unwrap());
        restarted.sidebar_model = app.sidebar_model.clone();
        restarted.worktree_owners.clear();
        restarted.workspace = Cagents::rebuild_workspace(&restarted.durable);
        restarted.workspace.resize_viewport(1_280.0);
        let _ = restarted.restore_worktree_scope();
        assert_eq!(
            restarted.workspace.order().collect::<Vec<_>>(),
            vec![source.id, reopened]
        );
        assert_eq!(restarted.workspace.focused(), Some(reopened));
        assert_eq!(restarted.worktree_owners[&source.id], root);
        assert_eq!(restarted.worktree_owners[&reopened], root);
        assert!(!restarted.workspace.order().any(|id| id == cloned));
    }

    #[test]
    fn ownership_uses_the_longest_component_root() {
        let roots = [Path::new("/repo"), Path::new("/repo/nested")];
        assert_eq!(
            worktree_root(Path::new("/repo/nested/src"), roots.into_iter()),
            Some(Path::new("/repo/nested"))
        );
        assert_eq!(
            worktree_root(Path::new("/repo/src"), roots.into_iter()),
            Some(Path::new("/repo"))
        );
        assert_eq!(
            worktree_root(Path::new("/repo-fix/src"), roots.into_iter()),
            None
        );
    }

    #[test]
    fn worktree_path_index_is_kept_until_its_roots_or_rows_change() {
        fn row(id: WorktreeId, path: &str) -> SidebarWorktree {
            SidebarWorktree::new(id, path.into(), None, false, false, None, None, None)
        }
        let (folder, repository) = (ProjectId::new(), ProjectId::new());
        let (folder_row, shared_row, nested_row) =
            (WorktreeId::new(), WorktreeId::new(), WorktreeId::new());
        let mut projects = vec![
            SidebarProject::new(
                folder,
                RepositoryId::new(),
                "Folder".into(),
                "/root".into(),
                vec![row(folder_row, "/root")],
                vec![],
            )
            .with_kind(ProjectKind::Folder),
            SidebarProject::new(
                repository,
                RepositoryId::new(),
                "Repository".into(),
                "/root".into(),
                vec![row(shared_row, "/root"), row(nested_row, "/root/nested")],
                vec![],
            ),
        ];
        let folder_key = projects[0].folder_key().unwrap();
        let mut known = KnownWorktreeRoots::from_iter([PathBuf::from("/known")]);
        let mut index = WorktreePathIndex::default();
        index.refresh(&known, &projects);
        index.refresh(&known, &projects);
        assert_eq!(
            index.generation(),
            1,
            "unchanged rows and roots keep the index"
        );
        // Both projects list /root, so it is a row of each; attention goes to
        // the first loaded row there, the folder's own.
        assert_eq!(
            index.rows(Path::new("/root")),
            [(folder, folder_row), (repository, shared_row)]
        );
        assert_eq!(
            index.first_row(Path::new("/root")),
            Some((folder_key.project_id, folder_key.worktree_id))
        );
        assert_eq!(
            (&index).resolve(Path::new("/root/nested/src")),
            Some(Path::new("/root/nested"))
        );
        assert_eq!(
            (&index).resolve(Path::new("/known/src")),
            Some(Path::new("/known"))
        );
        assert_eq!((&index).resolve(Path::new("/elsewhere")), None);

        projects[1].worktrees[1] = row(nested_row, "/root/moved");
        index.refresh(&known, &projects);
        assert_eq!(index.generation(), 2, "a moved row rebuilds the index");
        assert_eq!(index.rows(Path::new("/root/nested")), []);
        assert_eq!(
            (&index).resolve(Path::new("/root/moved/src")),
            Some(Path::new("/root/moved"))
        );

        known.insert("/admitted".into());
        index.refresh(&known, &projects);
        assert_eq!(
            index.generation(),
            3,
            "a newly known root rebuilds the index"
        );
        assert_eq!(
            (&index).resolve(Path::new("/admitted/src")),
            Some(Path::new("/admitted"))
        );

        let folder_project = projects.remove(0);
        index.refresh(&known, &projects);
        assert_eq!(
            index.generation(),
            4,
            "a removed project rebuilds the index"
        );
        assert_eq!(index.rows(Path::new("/root")), [(repository, shared_row)]);
        assert_eq!(
            index.first_row(Path::new("/root")),
            Some((repository, shared_row))
        );

        projects.push(folder_project);
        index.refresh(&known, &projects);
        assert_eq!(index.generation(), 5, "an added project rebuilds the index");
        assert_eq!(
            index.first_row(Path::new("/root")),
            Some((repository, shared_row)),
            "the first project listing a path owns its attention"
        );
        projects.swap(0, 1);
        index.refresh(&known, &projects);
        assert_eq!(
            index.generation(),
            6,
            "reordered projects rebuild the index"
        );
        assert_eq!(
            index.first_row(Path::new("/root")),
            Some((folder_key.project_id, folder_key.worktree_id))
        );

        projects[0].kind = ProjectKind::Git;
        index.refresh(&known, &projects);
        assert_eq!(
            index.generation(),
            7,
            "a folder becoming a repository rebuilds"
        );
        assert_eq!(
            index.first_row(Path::new("/root")),
            Some((folder, folder_row)),
            "without its folder key, the project's own row is first"
        );
        projects[1].kind = ProjectKind::Folder;
        index.refresh(&known, &projects);
        assert_eq!(
            index.generation(),
            8,
            "a repository becoming a folder rebuilds"
        );
        let became_folder = projects[1].folder_key().unwrap();
        assert_eq!(
            index.first_row(Path::new("/root")),
            Some((folder, folder_row))
        );
        projects.swap(0, 1);
        index.refresh(&known, &projects);
        assert_eq!(
            index.first_row(Path::new("/root")),
            Some((became_folder.project_id, became_folder.worktree_id)),
            "a folder's key comes before its rows"
        );
        assert_eq!(
            index.rows(Path::new("/root")),
            [(repository, shared_row), (folder, folder_row)]
        );
    }

    #[test]
    fn indexed_roots_match_component_matching_for_nested_and_relative_paths() {
        let roots = [
            Path::new("/repo"),
            Path::new("/repo/nested"),
            Path::new("/repo/nested/../other"),
            Path::new("relative"),
            Path::new("relative/child"),
            Path::new(""),
        ];
        let indexed = WorktreeRoots::new(roots.into_iter());
        for directory in [
            "/repo/nested/src",
            "/repo/src",
            "/repo-fix/src",
            "/repo/nested/../other/file",
            "relative/child/file",
            "relative-other/file",
            "",
        ] {
            let directory = Path::new(directory);
            assert_eq!(
                indexed.resolve(directory),
                worktree_root(directory, roots.into_iter()),
                "{directory:?}"
            );
        }
    }

    #[test]
    fn scope_switches_preserve_durable_cards_and_global_browser_descriptors() {
        let mut a = terminal("/repo/src");
        // Shell title updates do not move ownership into another worktree.
        a.title = "/repo/nested".into();
        let a_editor = repository("/repo");
        let b = terminal("/repo/nested/src");
        let b_editor = repository("/repo/nested");
        let mut browser = repository("/unused");
        browser.kind = CardKind::Browser(crate::model::BrowserSession {
            id: BrowserId::new(),
            current_url: Url::parse("https://example.com/current").unwrap(),
            history: vec![Url::parse("https://example.com/previous").unwrap()],
            history_index: 0,
            scroll_x: 12.0,
            scroll_y: 42.0,
            fidelity: BrowserRestoreFidelity::UrlOnly,
        });
        let state = WorkspaceState {
            cards: vec![
                a.clone(),
                a_editor.clone(),
                browser.clone(),
                b.clone(),
                b_editor.clone(),
            ]
            .into(),
            focused: Some(a.id),
            ..WorkspaceState::default()
        };
        let before = state.clone();
        let roots = [Path::new("/repo"), Path::new("/repo/nested")];
        let owners = HashMap::new();
        let a_view = scope_descriptor(&state, roots[0], &roots, &owners, Some(a.id));
        let b_view = scope_descriptor(&state, roots[1], &roots, &owners, Some(b.id));
        let returned = scope_descriptor(&state, roots[0], &roots, &owners, a_view.focused);
        assert_eq!(a_view.cards, vec![a, a_editor, browser.clone()].into());
        assert_eq!(b_view.cards, vec![browser, b, b_editor].into());
        assert_eq!(returned, a_view);
        assert_eq!(state, before);
    }

    #[test]
    fn insertion_anchors_to_durable_focus_across_hidden_cards() {
        let a = terminal("/a");
        let hidden = terminal("/b");
        let next = terminal("/a");
        let state = WorkspaceState {
            cards: vec![a.clone(), hidden.clone(), next.clone()].into(),
            focused: Some(a.id),
            ..WorkspaceState::default()
        };
        let roots = [Path::new("/a"), Path::new("/b")];
        let scoped = scope_descriptor(&state, roots[0], &roots, &HashMap::new(), Some(a.id));
        let workspace = Cagents::rebuild_workspace(&scoped);
        let indices = insertion_indices(workspace.cards(), &state.cards, Some(a.id));
        assert_eq!(indices, (1, 1));
        let added = terminal("/a");
        let mut inserted = state.clone();
        inserted.cards.insert(indices.1, added.clone());
        let restored =
            scope_descriptor(&inserted, roots[0], &roots, &HashMap::new(), Some(added.id));
        assert_eq!(restored.cards, vec![a, added, next].into());
        assert_eq!(inserted.cards[2], hidden);
        assert_eq!(
            insertion_indices(workspace.cards(), &state.cards, None),
            (2, 3)
        );
    }

    #[test]
    fn selecting_worktrees_restores_focus_without_creating_or_removing_terminals() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        let a_first = terminal("/repo/src");
        let a_focused = terminal("/repo");
        let b = terminal("/repo/nested/src");
        app.durable = Arc::new(WorkspaceState {
            cards: vec![a_first.clone(), b.clone(), a_focused.clone()].into(),
            focused: Some(a_focused.id),
            ..WorkspaceState::default()
        });
        *app.sidebar_model.projects = vec![SidebarProject::new(
            ProjectId::new(),
            RepositoryId::new(),
            "repo".into(),
            "/repo".into(),
            ["/repo", "/repo/nested", "/empty"]
                .into_iter()
                .map(|path| {
                    SidebarWorktree::new(
                        crate::model::WorktreeId::new(),
                        path.into(),
                        None,
                        false,
                        false,
                        None,
                        None,
                        None,
                    )
                })
                .collect(),
            Vec::new(),
        )];
        app.workspace = Cagents::rebuild_workspace(&app.durable);
        app.workspace.resize_viewport(1_280.0);
        let _ = app.restore_worktree_scope();
        assert_eq!(app.workspace.focused(), Some(a_focused.id));
        assert_eq!(app.terminal_directory(), Some(PathBuf::from("/repo")));
        assert_eq!(
            app.workspace.order().collect::<Vec<_>>(),
            vec![a_first.id, a_focused.id]
        );

        let _ = app.select_worktree("/repo/nested".into(), true);
        assert_eq!(app.workspace.order().collect::<Vec<_>>(), vec![b.id]);
        assert_eq!(app.workspace.focused(), Some(b.id));
        assert_eq!(
            app.terminal_directory(),
            Some(PathBuf::from("/repo/nested"))
        );
        assert_eq!(app.exposure(a_focused.id), Exposure::Offscreen);
        assert_eq!(
            app.sidebar_state.active_worktree().unwrap().canonical_path,
            PathBuf::from("/repo/nested")
        );

        // A restart reconstructs ownership from the saved launch directory,
        // including a legacy terminal launched below the registered root.
        let mut restarted = super::super::tests::context_inspector_test_app(false);
        restarted.durable = Arc::clone(&app.durable);
        restarted.sidebar_model = app.sidebar_model.clone();
        restarted.workspace = Cagents::rebuild_workspace(&restarted.durable);
        restarted.workspace.resize_viewport(1_280.0);
        let _ = restarted.restore_worktree_scope();
        assert_eq!(restarted.workspace.order().collect::<Vec<_>>(), vec![b.id]);
        assert_eq!(restarted.workspace.focused(), Some(b.id));
        assert_eq!(
            restarted.terminal_directory(),
            Some(PathBuf::from("/repo/nested"))
        );

        let _ = app.select_worktree("/repo".into(), true);
        assert_eq!(app.workspace.focused(), Some(a_focused.id));
        assert_eq!(
            app.workspace.order().collect::<Vec<_>>(),
            vec![a_first.id, a_focused.id]
        );
        assert_eq!(app.workspace.viewport_width(), 1_280.0);
        assert_eq!(app.exposure(b.id), Exposure::Offscreen);
        assert_eq!(
            app.durable
                .cards
                .iter()
                .map(|card| card.id)
                .collect::<Vec<_>>(),
            vec![a_first.id, b.id, a_focused.id]
        );
        assert!(
            app.durable
                .cards
                .iter()
                .all(|card| card.existence.is_open())
        );

        let _ = app.select_worktree("/empty".into(), false);
        assert!(app.workspace.cards().is_empty());
        assert_eq!(app.terminal_directory(), Some(PathBuf::from("/empty")));
        assert_eq!(
            app.sidebar_state.active_worktree().unwrap().canonical_path,
            PathBuf::from("/empty")
        );
        assert_eq!(app.durable.cards.len(), 3);
    }

    /// Opening worktrees from the sidebar changes where this window is and
    /// nothing else: the durable state follows each click at once, and the
    /// burst is handed to the writer once.
    #[test]
    fn opening_worktrees_from_the_sidebar_saves_once_after_the_burst() {
        use super::super::terminal_output_tests::persists;
        let mut app = super::super::tests::context_inspector_test_app(false);
        let a = terminal("/repo");
        let b = terminal("/repo/nested");
        app.durable = Arc::new(WorkspaceState {
            cards: vec![a.clone(), b.clone()].into(),
            focused: Some(a.id),
            ..WorkspaceState::default()
        });
        *app.sidebar_model.projects = vec![SidebarProject::new(
            ProjectId::new(),
            RepositoryId::new(),
            "repo".into(),
            "/repo".into(),
            ["/repo", "/repo/nested"]
                .into_iter()
                .map(|path| {
                    SidebarWorktree::new(
                        crate::model::WorktreeId::new(),
                        path.into(),
                        None,
                        false,
                        false,
                        None,
                        None,
                        None,
                    )
                })
                .collect(),
            Vec::new(),
        )];
        app.workspace = Cagents::rebuild_workspace(&app.durable);
        app.workspace.resize_viewport(1_280.0);
        let _ = app.restore_worktree_scope();

        let saved = persists();
        for (path, card) in [("/repo/nested", &b), ("/repo", &a), ("/repo/nested", &b)] {
            let project = &app.sidebar_model.projects[0];
            let worktree = project
                .worktrees
                .iter()
                .find(|worktree| worktree.path == std::path::Path::new(path))
                .unwrap();
            let open = SidebarAction::OpenWorktree {
                project_id: project.id,
                worktree_id: worktree.id,
                path: path.into(),
            };
            drop(app.update(Message::Sidebar(open)));
            assert_eq!(app.workspace.focused(), Some(card.id));
            assert_eq!(
                app.durable.active_worktree.as_deref(),
                Some(std::path::Path::new(path))
            );
            assert_eq!(persists(), saved, "a click alone waits for the burst");
        }
        drop(app.update(Message::NavigationSaveDue(app.navigation_save_generation)));
        assert_eq!(persists(), saved + 1, "the burst is saved once");
    }

    #[test]
    fn application_close_protects_unsaved_editors_in_hidden_worktrees() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        register_worktrees(&mut app, &["/a", "/b"]);
        let editor = repository("/a");
        app.durable = Arc::new(WorkspaceState {
            cards: vec![editor.clone(), terminal("/b")].into(),
            focused: Some(editor.id),
            ..WorkspaceState::default()
        });
        app.cards.insert(
            editor.id,
            CardRuntime::Repository(Box::new(RepositoryRuntime::dirty_fixture(editor.id))),
        );
        let _ = app.select_worktree("/b".into(), false);
        assert!(app.workspace.card(editor.id).is_none());
        let main = window::Id::unique();
        app.core.set_main_window_id(Some(main));
        let _ = app.update(Message::WindowCloseRequested(main));
        assert_eq!(
            app.dirty_repository_close.as_ref().map(|close| close.card),
            Some(editor.id)
        );
    }

    #[test]
    fn browser_focus_does_not_replace_persisted_worktree_selection() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        register_worktrees(&mut app, &["/a", "/b"]);
        let a = terminal("/a");
        let b = terminal("/b");
        let mut browser = terminal("/unused");
        browser.kind = CardKind::Browser(crate::model::BrowserSession {
            id: BrowserId::new(),
            current_url: Url::parse("https://example.invalid").unwrap(),
            history: vec![Url::parse("https://example.invalid").unwrap()],
            history_index: 0,
            scroll_x: 0.0,
            scroll_y: 0.0,
            fidelity: BrowserRestoreFidelity::UrlOnly,
        });
        app.durable = Arc::new(WorkspaceState {
            cards: vec![a.clone(), b.clone(), browser.clone()].into(),
            focused: Some(a.id),
            ..WorkspaceState::default()
        });
        let _ = app.restore_worktree_scope();
        let _ = app.select_worktree("/b".into(), false);
        let update = app.workspace.focus(browser.id).unwrap();
        let _ = app.apply_viewport(update, false, DurableChange::None);
        let mut restarted = super::super::tests::context_inspector_test_app(false);
        restarted.durable = Arc::clone(&app.durable);
        restarted.sidebar_model = app.sidebar_model.clone();
        let _ = restarted.restore_worktree_scope();
        assert_eq!(restarted.terminal_directory(), Some(PathBuf::from("/b")));
        assert_eq!(restarted.workspace.focused(), Some(browser.id));
        assert!(restarted.workspace.card(a.id).is_none());
        let _ = restarted.select_worktree("/a".into(), false);
        assert_eq!(restarted.workspace.focused(), Some(a.id));
    }

    #[test]
    fn a_created_nested_worktree_opens_in_its_own_channel_with_a_terminal_there() {
        let root = tempfile::tempdir().unwrap();
        let nested = root.path().join("worktrees/topic");
        let project = ProjectId::new();
        let mut app = super::super::tests::context_inspector_test_app(false);
        // A connected publisher keeps the new scope's seed pending until the
        // registry answers, as a running window's does.
        let (tx, _requests) = super::super::workspace_model_sync::test_model_channel();
        app.workspace_model_tx = tx;
        // These selection checks run after the shared registry has hydrated.
        app.workspace_model.identity = uuid::Uuid::new_v4();
        app.settings.default_channel_pane = settings::DefaultPane::Terminal;
        let parent_terminal = terminal(root.path().to_str().unwrap());
        app.durable = Arc::new(WorkspaceState {
            cards: vec![parent_terminal.clone()].into(),
            focused: Some(parent_terminal.id),
            ..WorkspaceState::default()
        });
        *app.sidebar_model.projects = vec![SidebarProject::new(
            project,
            RepositoryId::new(),
            "repo".into(),
            root.path().into(),
            vec![SidebarWorktree::new(
                crate::model::WorktreeId::new(),
                root.path().into(),
                Some("main".into()),
                false,
                false,
                None,
                None,
                None,
            )],
            Vec::new(),
        )];
        let _ = app.restore_worktree_scope();
        let _ = app.update(Message::WorktreeCreated {
            project_id: project,
            repository: root.path().into(),
            result: Ok(WorktreeCreationOutcome::Created {
                worktree: WorktreeRecord {
                    id: crate::model::WorktreeId::new(),
                    path: nested.clone(),
                    head_oid: None,
                    branch: Some("refs/heads/topic".into()),
                    detached: false,
                    bare: false,
                    lock: crate::repository::WorktreeLock::default(),
                    prunable_reason: None,
                    created_at_unix_ms: None,
                },
                warnings: Vec::new(),
            }),
        });
        // The parent's strip keeps its own pane, which is not the new
        // worktree's: the new worktree gets a channel of its own, which this
        // window made and so knows to be empty without waiting for the
        // registry.
        let [channel] = &app.durable.channels[..] else {
            panic!("the new worktree's channel");
        };
        assert_eq!(app.durable.active_channel, Some(channel.id));
        assert_eq!(channel.checkout.as_deref(), Some(nested.as_path()));
        assert_eq!(channel.title, "topic");
        assert_eq!(app.terminal_directory(), Some(nested.clone()));
        assert_eq!(app.durable.cards.len(), 2);
        assert!(app.workspace.card(parent_terminal.id).is_none());
        let terminal = app.card_state(app.workspace.focused().unwrap()).unwrap();
        assert!(
            matches!(&terminal.kind, CardKind::Terminal(session) if session.launch.working_directory == nested)
        );
    }

    /// The sidebar draws worktrees newest first, and a worktree the user has
    /// just made is the newest one there is. Its only creation evidence used
    /// to be the destination's filesystem birth time, which overlay and
    /// network mounts do not keep, so the created row reached the sidebar
    /// timeless, sorted after every row that had a time, and among the
    /// timeless rows fell back to path - which put a new `zulu-new` below an
    /// older `alpha-old` until a later reload happened to patch it from the
    /// journal. The creation instant now travels with the created record.
    #[test]
    fn a_created_worktree_renders_below_the_primary_and_above_an_older_sibling() {
        let root = tempfile::tempdir().unwrap();
        let repository = root.path().join("repo");
        std::fs::create_dir(&repository).unwrap();
        // Git records the destination it was given, and the journal settles
        // against canonical paths, so the fixture has to agree with both.
        let repository = repository.canonicalize().unwrap();
        let git = |arguments: &[&str]| {
            let output = std::process::Command::new("git")
                .arg("-C")
                .arg(&repository)
                .args(arguments)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&["init", "-b", "main"]);
        git(&[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--allow-empty",
            "-m",
            "base",
        ]);
        git(&["worktree", "add", "-b", "alpha-old", "worktrees/alpha-old"]);

        let service = RepositoryService::system("git");
        let store = crate::persistence::StateStore::open(
            root.path().join("state.sqlite"),
            crate::model::BootId::new("worktree-order-test"),
        )
        .unwrap();
        let destination = repository.join("worktrees/zulu-new");
        let request = CreateWorktreeRequest {
            resolution_warnings: Vec::new(),
            setup_intent: None,
            repository: repository.clone(),
            managed_root: repository.join("worktrees"),
            managed_ignore_root: None,
            destination: destination.clone(),
            branch: crate::repository::ValidatedBranchName::parse("zulu-new").unwrap(),
            base: crate::repository::BaseRevision::LocalBranch(
                crate::repository::ValidatedBranchName::parse("main").unwrap(),
            ),
            track: false,
            operation_id: crate::model::OperationId::new(),
        };
        let (progress, _progress) = async_channel::bounded(8);
        let report = service.create_worktree_journaled(
            &store,
            &request,
            1,
            &CancellationToken::new(),
            &progress,
        );
        assert_eq!(
            report.state,
            crate::repository::WorktreeOperationState::Created,
            "{report:?}"
        );

        // Stand in for a filesystem that keeps no birth time: strip the listed
        // time out of the durable journal and recover the operation from it,
        // so the creation instant can only reach the sidebar by being carried
        // through the outcome.
        let journaled = store.journaled_worktree_operations().unwrap().remove(0);
        let mut timeless = journaled.payload.clone();
        timeless["outcome"]["worktree"]["created_at_unix_ms"] = serde_json::Value::Null;
        store
            .update_operation(
                journaled.id,
                &journaled.payload,
                &timeless,
                journaled.status,
            )
            .unwrap();
        let report = service
            .rehydrate_worktree_operations(&store)
            .unwrap()
            .remove(0);

        let older = repository.join("worktrees/alpha-old");
        let project = ProjectId::new();
        let mut app = super::super::tests::context_inspector_test_app(false);
        *app.sidebar_model.projects = vec![SidebarProject::new(
            project,
            RepositoryId::new(),
            "repo".into(),
            repository.clone(),
            service
                .list_worktrees(&repository, &CancellationToken::new())
                .unwrap()
                .into_iter()
                .filter(|record| record.path != destination)
                .map(|mut record| {
                    // Same filesystem, so the rows Git listed are timeless too.
                    record.created_at_unix_ms = None;
                    SidebarWorktree::from_repository(&record, None)
                })
                .collect(),
            Vec::new(),
        )];
        let _ = app.restore_worktree_scope();
        let _ = app.update(Message::WorktreeCreated {
            project_id: project,
            repository: repository.clone(),
            result: Ok(report
                .outcome
                .expect("a created operation reports its outcome")),
        });

        let entry = &app.sidebar_model.projects[0];
        assert_eq!(
            sidebar::worktrees_in_display_order(
                &entry.repository,
                crate::settings::WorktreeSort::Newest,
                entry.worktrees.iter(),
            )
            .iter()
            .map(|row| row.path.as_path())
            .collect::<Vec<_>>(),
            vec![repository.as_path(), destination.as_path(), older.as_path()]
        );
    }
}
