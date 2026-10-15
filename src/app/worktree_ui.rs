use super::*;
use crate::model::ChannelId;

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

/// Resolve a directory by its ancestors rather than comparing it with every
/// historical checkout. Built for one immutable workspace projection.
pub(super) struct WorktreeRoots<'a> {
    roots: HashMap<&'a Path, &'a Path>,
}

impl<'a> WorktreeRoots<'a> {
    fn new(roots: impl Iterator<Item = &'a Path>) -> Self {
        Self {
            roots: roots.map(|root| (root, root)).collect(),
        }
    }

    fn resolve(&self, directory: &Path) -> Option<&'a Path> {
        directory
            .ancestors()
            .find_map(|ancestor| self.roots.get(ancestor).copied())
            .or_else(|| self.roots.get(Path::new("")).copied())
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
        CardKind::Browser(_) | CardKind::Settings | CardKind::Accounts | CardKind::Automations => {
            None
        }
    }
}

fn belongs_to_scope(
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
    let channel_cards = selected_channel.is_none().then(|| {
        state
            .channels
            .iter()
            .flat_map(|channel| channel.cards.iter().copied())
            .collect::<HashSet<_>>()
    });
    let cards = state
        .cards
        .iter()
        .filter(|card| {
            selected_channel.map_or_else(
                || {
                    !channel_cards
                        .as_ref()
                        .is_some_and(|cards| cards.contains(&card.id))
                        && belongs_to_scope(card, scope, roots, owners)
                },
                |channel| card.existence.is_open() && channel.cards.contains(&card.id),
            )
        })
        .cloned()
        .collect::<Vec<_>>();
    let focused = preferred_focus
        .filter(|id| cards.iter().any(|card| card.id == *id))
        .or_else(|| {
            cards
                .iter()
                .find(|card| matches!(&card.kind, CardKind::Terminal(_) | CardKind::Chat(_)))
                .map(|card| card.id)
        })
        .or_else(|| {
            cards
                .iter()
                .find(|card| matches!(&card.kind, CardKind::Repository(_)))
                .map(|card| card.id)
        })
        // A global-only strip still needs an active pane, without making that
        // browser an ownership source or displacing a worktree's own panes.
        .or_else(|| cards.first().map(|card| card.id));
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
        workspace_history: Vec::new(),
        repository_navigation: HashMap::new(),
        // A projection carries no channels, so it has nothing to say about what
        // was shared with which registry.
        workspace_model_identity: None,
        local_only_cards: HashSet::new(),
        pending_terminal_creations: HashSet::new(),
    }
}

fn insertion_indices(
    visible: &[WorkspaceCard],
    durable: &[CardState],
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
    /// Merge navigation into the full descriptor; never snapshot the projected
    /// strip over durable cards belonging to a hidden scope.
    pub(super) fn snapshot_workspace_scope(&mut self) {
        if self.active_workspace.is_none() {
            return;
        }
        let focused = self.workspace.focused();
        let active_order = self.durable.active_channel.and_then(|id| {
            self.durable
                .channels
                .iter()
                .find(|channel| channel.id == id)
                .map(|channel| {
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
                    (id, order)
                })
        });
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
        let needs_update = self.durable.active_worktree != self.active_workspace
            || self.durable.worktree_focus != self.worktree_focus
            || active_order.as_ref().is_some_and(|(id, order)| {
                self.durable
                    .channels
                    .iter()
                    .any(|channel| channel.id == *id && &channel.cards != order)
            })
            || self.durable.channels.iter().any(|channel| {
                (Some(channel.id) == self.durable.active_channel && channel.focused != focused)
                    || channel.cards.iter().any(|id| {
                        !self
                            .durable
                            .cards
                            .iter()
                            .any(|card| card.id == *id && card.existence.is_open())
                    })
            });
        if !needs_update {
            return;
        }
        let durable = Arc::make_mut(&mut self.durable);
        if let Some((id, order)) = active_order {
            if let Some(channel) = durable.channels.iter_mut().find(|channel| channel.id == id) {
                channel.cards = order;
            }
        }
        for channel in &mut durable.channels {
            channel.cards.retain(|id| {
                durable
                    .cards
                    .iter()
                    .any(|card| card.id == *id && card.existence.is_open())
            });
            if !channel
                .focused
                .is_some_and(|id| channel.cards.contains(&id))
            {
                channel.focused = channel.cards.first().copied();
            }
        }
        if let Some(id) = durable.active_channel {
            if let Some(channel) = durable.channels.iter_mut().find(|channel| channel.id == id) {
                channel.focused = focused.filter(|card| channel.cards.contains(card));
            }
        }
        durable.active_worktree.clone_from(&self.active_workspace);
        durable.worktree_focus.clone_from(&self.worktree_focus);
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
            {
                if let Some(path) = &self.active_workspace {
                    Arc::make_mut(&mut self.durable)
                        .card_scopes
                        .insert(card, path.clone());
                    self.worktree_owners.insert(card, path.clone());
                }
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

    pub(super) fn select_channel(&mut self, id: ChannelId) -> Task<Message> {
        let Some(channel) = self
            .durable
            .channels
            .iter()
            .find(|channel| channel.id == id)
        else {
            return Task::none();
        };
        let Some(project) = self.sidebar_model.project(channel.project_id) else {
            return Task::none();
        };
        let path = project
            .worktrees
            .iter()
            .find(|worktree| sidebar::is_primary_worktree(&project.repository, &worktree.path))
            .map(|worktree| worktree.path.clone())
            .unwrap_or_else(|| {
                project
                    .repository
                    .parent()
                    .filter(|parent| sidebar::is_primary_worktree(&project.repository, parent))
                    .unwrap_or(&project.repository)
                    .to_path_buf()
            });
        if self.durable.active_channel == Some(id)
            && self.active_workspace.as_ref() == Some(&path)
            && self.workspace.order().eq(self
                .durable
                .cards
                .iter()
                .filter(|card| card.existence.is_open() && channel.cards.contains(&card.id))
                .map(|card| card.id))
        {
            return Task::none();
        }
        self.snapshot_workspace_scope();
        let discarded = if self.durable.active_channel != Some(id) {
            self.discard_unused_default_chats()
        } else {
            false
        };
        Arc::make_mut(&mut self.durable).active_channel = Some(id);
        let focused = self
            .durable
            .channels
            .iter()
            .find(|channel| channel.id == id)
            .and_then(|channel| channel.focused);
        let scoped = scope_descriptor(&self.durable, &path, &[], &self.worktree_owners, focused);
        let offset = self.channel_offsets.get(&id).copied();
        let scope = self.install_workspace_scope(scoped, path, offset);
        if discarded {
            self.persist();
        }
        scope
    }

    /// Resolve filesystem aliases only when changing/restoring scope, never in
    /// the render path or by rewriting a hosted session's durable launch recipe.
    fn refresh_worktree_owners(&mut self, selected: Option<&Path>) -> Option<PathBuf> {
        // Deleted worktree rows disappear from navigation, not from ownership.
        // Otherwise a surviving nested session would fall into its parent repo.
        self.known_worktree_roots
            .extend(self.sidebar_model.projects.iter().flat_map(|project| {
                project
                    .worktrees
                    .iter()
                    .map(|worktree| worktree.path.clone())
            }));
        self.known_worktree_roots.extend(
            self.sidebar_model
                .projects
                .iter()
                .filter(|project| project.kind == ProjectKind::Folder)
                .map(|project| project.repository.clone()),
        );
        self.known_worktree_roots
            .extend(
                self.durable
                    .cards
                    .iter()
                    .filter_map(|card| match &card.kind {
                        CardKind::Repository(view) => Some(view.worktree.clone()),
                        _ => None,
                    }),
            );
        let roots = self
            .known_worktree_roots
            .iter()
            .map(|path| {
                (
                    path.as_path(),
                    if crate::device::is_remote() {
                        path.clone()
                    } else {
                        path.canonicalize().unwrap_or_else(|_| path.clone())
                    },
                    self.sidebar_model.worktree_at(path).is_some(),
                )
            })
            .collect::<Vec<_>>();
        let resolve = |directory: &Path| {
            let canonical = if crate::device::is_remote() {
                directory.to_path_buf()
            } else {
                directory
                    .canonicalize()
                    .unwrap_or_else(|_| directory.into())
            };
            roots
                .iter()
                .filter(|(_, root, _)| canonical.starts_with(root))
                .max_by_key(|(_, root, visible)| (root.components().count(), *visible))
                .map_or_else(|| canonical.clone(), |(root, _, _)| (*root).to_path_buf())
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
    /// This runs after every update, terminal output included, so channel
    /// membership is collected once rather than rescanned per card.
    pub(super) fn refresh_sidebar_worktree_panes(&mut self) {
        let channeled = self
            .durable
            .channels
            .iter()
            .flat_map(|channel| channel.cards.iter().copied())
            .collect::<HashSet<_>>();
        let occupied = self
            .durable
            .cards
            .iter()
            .filter(|card| card.existence.is_open() && !channeled.contains(&card.id))
            .filter_map(|card| self.unchanneled_card_worktree(card))
            .collect::<HashSet<_>>();
        let worktrees = self
            .sidebar_model
            .projects
            .iter()
            .flat_map(|project| {
                project
                    .worktrees
                    .iter()
                    .filter(|worktree| occupied.contains(worktree.path.as_path()))
                    .map(move |worktree| (project.id, worktree.id))
            })
            .collect();
        self.sidebar_state.set_worktrees_with_panes(worktrees);
    }

    pub(super) fn card_worktree<'a>(&'a self, card: &'a CardState) -> Option<&'a Path> {
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
        WorktreeRoots::new(
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
    }

    pub(super) fn unchanneled_card_worktree_indexed<'a>(
        &'a self,
        card: &'a CardState,
        roots: &WorktreeRoots<'a>,
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

    /// A project opens in its first worktree, or its repository without one.
    pub(super) fn project_entry_directory(&self, project: ProjectId, repository: &Path) -> PathBuf {
        self.sidebar_model
            .projects
            .iter()
            .find(|row| row.id == project)
            .and_then(|row| row.worktrees.first())
            .map_or_else(
                || repository.to_path_buf(),
                |worktree| worktree.path.clone(),
            )
    }

    pub(super) fn select_worktree(
        &mut self,
        path: PathBuf,
        create_if_empty: bool,
    ) -> Task<Message> {
        let path = self
            .refresh_worktree_owners(Some(&path))
            .expect("selected scope");
        if let Err(error) = self.worktree_protections.protect_next(&path) {
            self.status = Some(TransientNotice::warning(format!(
                "Cannot open this worktree yet: {error}"
            )));
            return Task::none();
        }
        let roots = self
            .known_worktree_roots
            .iter()
            .map(PathBuf::as_path)
            .collect::<Vec<_>>();
        // Ownership and selected scope were resolved against the same canonical roots.
        let has_terminal = self.durable.cards.iter().any(|card| {
            card.existence.is_open()
                && matches!(&card.kind, CardKind::Terminal(_) | CardKind::Chat(_))
                && !self
                    .durable
                    .channels
                    .iter()
                    .any(|channel| channel.cards.contains(&card.id))
                && self
                    .card_worktree(card)
                    .is_some_and(|directory| directory == path)
        });
        let was_channel = self.durable.active_channel.is_some();
        if self.active_workspace.as_ref() == Some(&path)
            && !was_channel
            && self.workspace.order().eq(self
                .durable
                .cards
                .iter()
                .filter(|card| {
                    !self
                        .durable
                        .channels
                        .iter()
                        .any(|channel| channel.cards.contains(&card.id))
                        && belongs_to_scope(card, &path, &roots, &self.worktree_owners)
                })
                .map(|card| card.id))
        {
            if !create_if_empty {
                return Task::none();
            }
            return if !has_terminal {
                self.add_default_pane(path, self.settings.default_worktree_pane)
            } else {
                Task::none()
            };
        }
        let promoted = !was_channel
            && self.active_workspace.as_ref().is_some_and(|previous| {
                previous != &path
                    && self.durable.cards.iter().any(|card| {
                        card.existence.is_open()
                            && card_directory(card) == Some(previous.as_path())
                            && self.card_worktree(card) == Some(path.as_path())
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
        if was_channel {
            self.snapshot_workspace_scope();
        }
        let discarded =
            if !promoted && (was_channel || self.active_workspace.as_ref() != Some(&path)) {
                self.discard_unused_default_chats()
            } else {
                false
            };
        Arc::make_mut(&mut self.durable).active_channel = None;
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
        let scope = self.install_workspace_scope(scoped, path.clone(), offset);
        let terminal = if create_if_empty && !has_terminal {
            self.add_default_pane(path, self.settings.default_worktree_pane)
        } else {
            Task::none()
        };
        if discarded {
            self.persist();
        }
        Task::batch([scope, terminal])
    }

    fn install_workspace_scope(
        &mut self,
        scoped: WorkspaceState,
        path: PathBuf,
        offset: Option<f32>,
    ) -> Task<Message> {
        let width = self.workspace.viewport_width();
        // Hide before replacing the strip: its former widgets no longer get a
        // chance to send native-surface visibility updates after unmounting.
        self.set_workspace_surfaces_visible(false);
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
        // Focus selects the initial position only on the first visit. Returning
        // to a strip restores its actual offset, even if focus is offscreen.
        let offset = offset.unwrap_or_else(|| self.workspace.camera().target());
        self.workspace.native_scroll(offset, width);
        let transitions = self
            .durable
            .cards
            .iter()
            .filter(|card| card.existence.is_open())
            .map(|card| VisibilityTransition {
                id: card.id,
                from: match card.exposure {
                    Exposure::Visible => VisibilityClass::Visible,
                    Exposure::Prefetch => VisibilityClass::Overscan,
                    Exposure::Offscreen => VisibilityClass::Offscreen,
                },
                to: self
                    .workspace
                    .visibility(card.id)
                    .unwrap_or(VisibilityClass::Offscreen),
            })
            .collect();
        let viewport = self.apply_viewport(
            ViewportUpdate {
                offset_changed: true,
                transitions,
            },
            true,
            true,
        );
        // The strip being left keeps its visuals for the usual grace, so
        // coming straight back finds its panes as they were instead of
        // restoring every one of them. The panes now on screen get their
        // slots regardless: a visible card takes one from the hidden card
        // seen longest ago when the budget is full.
        let residency = self.lifecycle_sweep();
        let source_control = self.rebind_source_control();
        Task::batch([viewport, residency, source_control])
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
        if let Some(path) = &self.active_workspace {
            return Some(path.clone());
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

    /// Must run before initialize_runtimes so hidden restored terminals are
    /// admitted as offscreen, not with another worktree's persisted exposure.
    pub(super) fn restore_worktree_scope(&mut self) -> Task<Message> {
        Arc::make_mut(&mut self.durable).normalize_channels();
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
            return self.select_worktree(scope, false);
        }
        let focused = self.durable.focused.and_then(|id| {
            self.durable
                .cards
                .iter()
                .find(|card| card.id == id && card.existence.is_open())
        });
        let owner = focused
            .filter(|card| self.card_worktree(card).is_some())
            .or_else(|| {
                self.durable
                    .cards
                    .iter()
                    .find(|card| card.existence.is_open() && self.card_worktree(card).is_some())
            });

        let Some(owner) = owner else {
            return Task::none();
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

    pub(super) fn terminal_insertion_indices(&self) -> (usize, usize) {
        insertion_indices(
            self.workspace.cards(),
            &self.durable.cards,
            self.workspace.focused(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{BrowserId, BrowserRestoreFidelity, ProjectChannel};

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
            ],
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
            cards: vec![main.clone(), first.clone(), focused.clone(), second.clone()],
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
            cards: vec![main.clone(), closed.clone()],
            channels: vec![ProjectChannel {
                title_source: Default::default(),
                id,
                project_id,
                title: "Channel".into(),
                ordinal: 0,
                shared: false,
                cards: vec![closed.id],
                focused: Some(closed.id),
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
            exposure: Exposure::Visible,
            residency: Residency::Cold,
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
            cards: vec![root.clone(), nested.clone(), channeled.clone()],
            channels: vec![ProjectChannel {
                title_source: Default::default(),
                id: ChannelId::new(),
                project_id,
                title: "Channel".into(),
                ordinal: 0,
                shared: false,
                cards: vec![channeled.id],
                focused: Some(channeled.id),
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
            cards: vec![shell],
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
        app.context_inspector_open = false;
        register_worktrees(&mut app, &["/repo", "/repo/worktree"]);
        let main = terminal("/repo");
        let nested = terminal("/repo/worktree");
        let channeled = terminal("/home/user");
        let channel = ChannelId::new();
        app.durable = Arc::new(WorkspaceState {
            cards: vec![main.clone(), nested.clone(), channeled.clone()],
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
        app.sidebar_model.projects = vec![SidebarProject::new(
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
            assert_eq!(
                matches!(
                    app.card_state(channel_card).unwrap().kind,
                    CardKind::Chat(_)
                ),
                channel == settings::DefaultPane::NativeChat
            );
            let _ = app.select_worktree("/repo".into(), true);
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
            },
            proof: None,
            terminal: Some(source),
        });
        app.durable = Arc::new(WorkspaceState {
            cards: vec![card.clone()],
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
            cards: vec![a, b],
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
                workspace_widget::Events { viewport: |_| () },
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
            cards: vec![a[0].clone(), b.clone(), a[1].clone(), a[2].clone()],
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
                .all(|card| app.card_state(card.id).unwrap().exposure == Exposure::Offscreen)
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
            cards: vec![a[0].clone(), a[1].clone(), a[2].clone(), b.clone()],
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
        assert_eq!(app.card_state(b.id).unwrap().exposure, Exposure::Offscreen);
        assert_eq!(app.workspace.camera().offset(), 250.0);
        assert_eq!(app.workspace.focused(), Some(a[0].id));
    }

    #[test]
    fn file_selection_focuses_its_pane_and_only_dismisses_an_overlay_sidebar() {
        for width in [1_600.0, design::WIDE_BREAKPOINT - 1.0] {
            let mut app = super::super::tests::context_inspector_test_app(false);
            register_worktrees(&mut app, &["/a", "/b"]);
            let editor = repository("/a");
            let shell = terminal("/a");
            app.durable = Arc::new(WorkspaceState {
                cards: vec![editor.clone(), shell.clone()],
                focused: Some(shell.id),
                ..WorkspaceState::default()
            });
            app.window_width = width;
            app.context_tab = ContextTab::Changes;
            let _ = app.restore_worktree_scope();
            let key = app.sidebar_state.active_worktree().unwrap().clone();
            let was_overlay = app.context_inspector_is_overlay();
            let _ = app.route_panel_open(&key, &RepoPath::parse("src/main.rs").unwrap());
            assert_eq!(app.workspace.focused(), Some(editor.id));
            assert_eq!(app.context_tab, ContextTab::Changes);
            assert_eq!(app.context_inspector_open, !was_overlay);
            if was_overlay {
                assert!(app.workspace_inputs_blocked());
                let transition = app.context_inspector_transition.unwrap();
                let _ = app.advance_context_inspector_transition(
                    transition.started_at + transition.duration(),
                );
            }
            assert!(!app.workspace_inputs_blocked());

            let _ = app.select_worktree("/b".into(), false);
            app.pending_panel_open = None;
            let _ = app.route_panel_open(&key, &RepoPath::parse("src/main.rs").unwrap());
            assert!(app.pending_panel_open.is_none());
            assert!(app.workspace.cards().is_empty());
        }
    }

    #[test]
    fn legacy_unscoped_terminals_remain_accessible_without_owning_new_worktrees() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        // These selection checks run after the shared registry has hydrated.
        app.workspace_model.identity = uuid::Uuid::new_v4();
        app.settings.default_worktree_pane = settings::DefaultPane::Terminal;
        let legacy = terminal("/home/user");
        let owned = terminal("/repo");
        app.durable = Arc::new(WorkspaceState {
            cards: vec![legacy.clone(), owned.clone()],
            focused: Some(owned.id),
            ..WorkspaceState::default()
        });
        register_worktrees(&mut app, &["/repo", "/other"]);

        let _ = app.select_worktree("/repo".into(), false);
        assert!(app.workspace.card(legacy.id).is_none());
        let _ = app.select_worktree("/other".into(), true);
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
            cards: vec![shell.clone(), editor.clone()],
            ..WorkspaceState::default()
        });
        let _ = app.select_worktree(alias.join("worktrees/feature"), false);
        assert_eq!(app.active_workspace.as_deref(), Some(nested.as_path()));
        assert_eq!(app.workspace.order().collect::<Vec<_>>(), vec![shell.id]);
        assert_eq!(app.card_state(shell.id).unwrap().kind, shell.kind);
        let _ = app.select_worktree(alias, false);
        assert_eq!(app.workspace.order().collect::<Vec<_>>(), vec![editor.id]);
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
            cards: vec![shell.clone()],
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
                cards: vec![parent.clone(), retained.clone()],
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
            assert_eq!(
                app.card_state(retained.id).unwrap().exposure,
                Exposure::Offscreen
            );
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
            cards: vec![source.clone()],
            focused: Some(source.id),
            active_worktree: Some(root.clone()),
            card_scopes: HashMap::from([(source.id, root.clone())]),
            ..WorkspaceState::default()
        });
        app.sidebar_model.projects = vec![SidebarProject::new(
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
            ],
            focused: Some(a.id),
            ..WorkspaceState::default()
        };
        let before = state.clone();
        let roots = [Path::new("/repo"), Path::new("/repo/nested")];
        let owners = HashMap::new();
        let a_view = scope_descriptor(&state, roots[0], &roots, &owners, Some(a.id));
        let b_view = scope_descriptor(&state, roots[1], &roots, &owners, Some(b.id));
        let returned = scope_descriptor(&state, roots[0], &roots, &owners, a_view.focused);
        assert_eq!(a_view.cards, vec![a, a_editor, browser.clone()]);
        assert_eq!(b_view.cards, vec![browser, b, b_editor]);
        assert_eq!(returned, a_view);
        assert_eq!(state, before);
    }

    #[test]
    fn insertion_anchors_to_durable_focus_across_hidden_cards() {
        let a = terminal("/a");
        let hidden = terminal("/b");
        let next = terminal("/a");
        let state = WorkspaceState {
            cards: vec![a.clone(), hidden.clone(), next.clone()],
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
        assert_eq!(restored.cards, vec![a, added, next]);
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
            cards: vec![a_first.clone(), b.clone(), a_focused.clone()],
            focused: Some(a_focused.id),
            ..WorkspaceState::default()
        });
        app.sidebar_model.projects = vec![SidebarProject::new(
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
        assert_eq!(
            app.card_state(a_focused.id).unwrap().exposure,
            Exposure::Offscreen
        );
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
        assert_eq!(app.card_state(b.id).unwrap().exposure, Exposure::Offscreen);
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

    #[test]
    fn application_close_protects_unsaved_editors_in_hidden_worktrees() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        register_worktrees(&mut app, &["/a", "/b"]);
        let editor = repository("/a");
        app.durable = Arc::new(WorkspaceState {
            cards: vec![editor.clone(), terminal("/b")],
            focused: Some(editor.id),
            ..WorkspaceState::default()
        });
        app.cards.insert(
            editor.id,
            CardRuntime::Repository(Box::new(RepositoryRuntime::dirty_fixture(editor.id))),
        );
        let _ = app.select_worktree("/b".into(), false);
        assert!(app.workspace.card(editor.id).is_none());
        let _ = app.update(Message::WindowCloseRequested(window::Id::unique()));
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
            cards: vec![a.clone(), b.clone(), browser.clone()],
            focused: Some(a.id),
            ..WorkspaceState::default()
        });
        let _ = app.restore_worktree_scope();
        let _ = app.select_worktree("/b".into(), false);
        let update = app.workspace.focus(browser.id).unwrap();
        let _ = app.apply_viewport(update, false, false);
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
    fn a_created_nested_worktree_gets_its_own_terminal_and_git_context() {
        let root = tempfile::tempdir().unwrap();
        let nested = root.path().join("worktrees/topic");
        let project = ProjectId::new();
        let mut app = super::super::tests::context_inspector_test_app(false);
        // These selection checks run after the shared registry has hydrated.
        app.workspace_model.identity = uuid::Uuid::new_v4();
        app.settings.default_worktree_pane = settings::DefaultPane::Terminal;
        let parent_terminal = terminal(root.path().to_str().unwrap());
        app.durable = Arc::new(WorkspaceState {
            cards: vec![parent_terminal.clone()],
            focused: Some(parent_terminal.id),
            ..WorkspaceState::default()
        });
        app.sidebar_model.projects = vec![SidebarProject::new(
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
        assert_eq!(app.terminal_directory(), Some(nested.clone()));
        assert_eq!(
            app.sidebar_state.active_worktree().unwrap().canonical_path,
            nested
        );
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
            setup_intent: None,
            repository: repository.clone(),
            managed_root: repository.join("worktrees"),
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
        app.sidebar_model.projects = vec![SidebarProject::new(
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
