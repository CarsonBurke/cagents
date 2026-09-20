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

/// Missing directories are not navigable worktrees. A permission or transient
/// I/O failure is not evidence of deletion, so retain those rows for recovery.
pub(super) fn worktree_directory_available(path: &Path) -> bool {
    match path.metadata() {
        Ok(metadata) => metadata.is_dir(),
        Err(error) => !matches!(
            error.kind(),
            std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
        ),
    }
}

fn card_directory(card: &CardState) -> Option<&Path> {
    match &card.kind {
        CardKind::Terminal(session) => Some(&session.launch.working_directory),
        CardKind::Chat(session) => Some(&session.launch.recipe.working_directory),
        CardKind::Repository(view) => Some(&view.worktree),
        CardKind::Browser(_) | CardKind::PullRequests(_) => None,
    }
}

fn belongs_to_scope(
    card: &CardState,
    scope: &Path,
    roots: &[&Path],
    owners: &HashMap<CardId, PathBuf>,
) -> bool {
    card.existence.is_open()
        && card_directory(card).is_none_or(|directory| {
            owners
                .get(&card.id)
                .map(PathBuf::as_path)
                .unwrap_or_else(|| {
                    worktree_root(directory, roots.iter().copied()).unwrap_or(directory)
                })
                == scope
        })
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
        // A projection carries no channels, so it has nothing to say about what
        // was shared with which registry.
        workspace_model_identity: None,
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
        Arc::make_mut(&mut self.durable).active_channel = Some(id);
        let focused = self
            .durable
            .channels
            .iter()
            .find(|channel| channel.id == id)
            .and_then(|channel| channel.focused);
        let scoped = scope_descriptor(&self.durable, &path, &[], &self.worktree_owners, focused);
        let offset = self.channel_offsets.get(&id).copied();
        self.install_workspace_scope(scoped, path, offset)
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
                    path.canonicalize().unwrap_or_else(|_| path.clone()),
                    self.sidebar_model.worktree_at(path).is_some(),
                )
            })
            .collect::<Vec<_>>();
        let resolve = |directory: &Path| {
            let canonical = directory
                .canonicalize()
                .unwrap_or_else(|_| directory.into());
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
                // An explicit filing outranks discovery: it exists precisely
                // because no root contains this card's launch directory.
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

    pub(super) fn card_worktree<'a>(&'a self, card: &'a CardState) -> Option<&'a Path> {
        if self
            .durable
            .channels
            .iter()
            .any(|channel| channel.cards.contains(&card.id))
        {
            return None;
        }
        let directory = card_directory(card)?;
        if let Some(scope) = self
            .worktree_owners
            .get(&card.id)
            .or_else(|| self.durable.card_scopes.get(&card.id))
        {
            return Some(scope.as_path());
        }
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

    pub(super) fn select_worktree(
        &mut self,
        path: PathBuf,
        create_if_empty: bool,
    ) -> Task<Message> {
        let path = self
            .refresh_worktree_owners(Some(&path))
            .expect("selected scope");
        let roots = self
            .known_worktree_roots
            .iter()
            .map(PathBuf::as_path)
            .collect::<Vec<_>>();
        // Ownership and selected scope were resolved against the same canonical roots.
        let has_terminal = self.durable.cards.iter().any(|card| {
            card.existence.is_open()
                && matches!(&card.kind, CardKind::Terminal(_) | CardKind::Chat(_))
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
                self.add_terminal(path)
            } else {
                Task::none()
            };
        }
        if !was_channel && let Some(previous) = &self.active_workspace {
            // Repository discovery can promote an unregistered launch directory
            // into its parent root. Carry its camera/focus into that same scope.
            let promoted = previous != &path
                && self.durable.cards.iter().any(|card| {
                    card_directory(card) == Some(previous.as_path())
                        && self.card_worktree(card) == Some(path.as_path())
                });
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
        let offset = self.worktree_offsets.get(&path).copied();
        let scope = self.install_workspace_scope(scoped, path.clone(), offset);
        let terminal = if create_if_empty && !has_terminal {
            self.add_terminal(path)
        } else {
            Task::none()
        };
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
        let residency = self.reconcile_scoped_lifecycle();
        let source_control = self.rebind_source_control();
        Task::batch([viewport, residency, source_control])
    }

    pub(super) fn reconcile_scoped_lifecycle(&mut self) -> Task<Message> {
        // Scope changes release hidden visuals immediately, never their hosted
        // sessions. A second pass admits visible cards after slots are freed.
        self.lifecycle.update_budget(LifecycleBudget {
            max_hot_cards: self.settings.max_hot_cards,
            offscreen_grace: Duration::ZERO,
        });
        let eviction = self.lifecycle_sweep();
        self.synchronize_lifecycle_budget();
        let admission = self.lifecycle_sweep();
        Task::batch([eviction, admission])
    }

    pub(super) fn terminal_directory(&self) -> Option<PathBuf> {
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
                    id: a,
                    project_id,
                    title: "A".into(),
                    ordinal: 0,
                    shared: false,
                    cards: vec![first.id, focused.id],
                    focused: Some(focused.id),
                },
                ProjectChannel {
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
                project: ProjectId::new(),
                repository: path.into(),
                worktree: path.into(),
            }),
        }
    }

    fn terminal(path: &str) -> CardState {
        let mut card = repository(path);
        let launch = LaunchRecipe {
            program: "/bin/sh".into(),
            arguments: Vec::new(),
            working_directory: path.into(),
            environment: Default::default(),
        };
        card.kind = CardKind::Terminal(TerminalSession {
            recovery_origin: None,
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
    fn chat_owns_nested_scope_and_prevents_duplicate_terminal_creation() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        register_worktrees(&mut app, &["/repo", "/repo/nested", "/other"]);
        let mut card = terminal("/repo/nested/src");
        let CardKind::Terminal(source) = card.kind else {
            unreachable!()
        };
        card.kind = CardKind::Chat(crate::chat::Session {
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
                true,
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
