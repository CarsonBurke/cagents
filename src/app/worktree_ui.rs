use super::*;

/// Ownership comes from the launch recipe, never a shell's current directory or
/// OSC title. Component-aware matching also keeps `repo-fix` outside `repo`.
fn worktree_root<'a>(
    directory: &'a Path,
    roots: impl Iterator<Item = &'a Path>,
) -> Option<&'a Path> {
    roots
        .filter(|root| directory.starts_with(root))
        .max_by_key(|root| root.components().count())
}

fn card_directory(card: &CardState) -> Option<&Path> {
    match &card.kind {
        CardKind::Terminal(session) => Some(&session.launch.working_directory),
        CardKind::Repository(view) => Some(&view.worktree),
        CardKind::Browser(_) | CardKind::PullRequests(_) => None,
    }
}

fn belongs_to_scope(card: &CardState, scope: &Path, roots: &[&Path]) -> bool {
    card.existence.is_open()
        && card_directory(card).is_none_or(|directory| {
            // Older sessions could launch outside Git repositories. Keep those
            // cards reachable until closed or their owning worktree is registered.
            worktree_root(directory, roots.iter().copied()).is_none_or(|root| root == scope)
        })
}

/// A view descriptor, not a replacement for the persisted workspace. Global
/// cards stay available; closing a card and hiding its worktree are unrelated.
fn scope_descriptor(
    state: &WorkspaceState,
    scope: &Path,
    roots: &[&Path],
    preferred_focus: Option<CardId>,
) -> WorkspaceState {
    let cards = state
        .cards
        .iter()
        .filter(|card| belongs_to_scope(card, scope, roots))
        .cloned()
        .collect::<Vec<_>>();
    let focused = preferred_focus
        .filter(|id| cards.iter().any(|card| card.id == *id))
        .or_else(|| {
            cards
                .iter()
                .find(|card| matches!(&card.kind, CardKind::Terminal(_)))
                .map(|card| card.id)
        })
        .or_else(|| {
            cards
                .iter()
                .find(|card| matches!(&card.kind, CardKind::Repository(_)))
                .map(|card| card.id)
        });
    WorkspaceState {
        schema_version: state.schema_version,
        id: state.id,
        generation: state.generation,
        existence: state.existence.clone(),
        cards,
        focused,
        active_worktree: None,
        worktree_focus: HashMap::new(),
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
    pub(super) fn select_worktree(
        &mut self,
        path: PathBuf,
        create_if_empty: bool,
    ) -> Task<Message> {
        let roots = self
            .sidebar_model
            .projects
            .iter()
            .flat_map(|project| &project.worktrees)
            .map(|worktree| worktree.path.as_path())
            .collect::<Vec<_>>();
        let path = worktree_root(&path, roots.iter().copied())
            .unwrap_or(&path)
            .to_path_buf();
        if let Some(previous) = &self.active_workspace
            && let Some(focused) = self.workspace.focused()
        {
            self.worktree_focus.insert(previous.clone(), focused);
        }
        let scoped = scope_descriptor(
            &self.durable,
            &path,
            &roots,
            self.worktree_focus.get(&path).copied(),
        );
        let has_terminal = scoped.cards.iter().any(|card| match &card.kind {
            CardKind::Terminal(session) => {
                let directory = session.launch.working_directory.as_path();
                worktree_root(directory, roots.iter().copied()).unwrap_or(directory) == path
            }
            _ => false,
        });
        let width = self.workspace.viewport_width();
        // Hide before replacing the strip: its former widgets no longer get a
        // chance to send native-surface visibility updates after unmounting.
        self.set_workspace_surfaces_visible(false);
        self.active_workspace = Some(path.clone());
        self.workspace = Self::rebuild_workspace(&scoped);
        self.workspace.resize_viewport(width);
        // Rebuilding starts a fresh camera. Jump directly to its focus target
        // instead of briefly presenting another worktree's first column.
        let target = self.workspace.camera().target();
        self.workspace.native_scroll(target, width);
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
        let terminal = if create_if_empty && !has_terminal {
            self.add_terminal(path)
        } else {
            Task::none()
        };
        Task::batch([viewport, residency, source_control, terminal])
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
        self.worktree_focus.clone_from(&self.durable.worktree_focus);
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
            .filter(|card| card_directory(card).is_some())
            .or_else(|| {
                self.durable
                    .cards
                    .iter()
                    .find(|card| card.existence.is_open() && card_directory(card).is_some())
            });
        let Some(owner) = owner else {
            return Task::none();
        };
        let directory = card_directory(owner).expect("selected card has a directory");
        let root = worktree_root(
            directory,
            self.sidebar_model
                .projects
                .iter()
                .flat_map(|project| &project.worktrees)
                .map(|worktree| worktree.path.as_path()),
        )
        .unwrap_or(directory)
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
    use crate::model::{BrowserId, BrowserRestoreFidelity};

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
        assert!(app.workspace.card(legacy.id).is_some());
        let _ = app.select_worktree("/other".into(), true);
        assert!(app.workspace.card(legacy.id).is_some());
        assert!(app.workspace.card(owned.id).is_none());
        let focused = app.card_state(app.workspace.focused().unwrap()).unwrap();
        assert!(matches!(&focused.kind, CardKind::Terminal(session)
            if session.launch.working_directory == Path::new("/other")));
        assert_ne!(focused.id, legacy.id);
        assert!(app.workspace.focus(legacy.id).is_ok());
        assert_eq!(app.terminal_directory(), Some(PathBuf::from("/other")));
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
        let a_view = scope_descriptor(&state, roots[0], &roots, Some(a.id));
        let b_view = scope_descriptor(&state, roots[1], &roots, Some(b.id));
        let returned = scope_descriptor(&state, roots[0], &roots, a_view.focused);
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
        let scoped = scope_descriptor(&state, roots[0], &roots, Some(a.id));
        let workspace = Cagents::rebuild_workspace(&scoped);
        let indices = insertion_indices(workspace.cards(), &state.cards, Some(a.id));
        assert_eq!(indices, (1, 1));
        let added = terminal("/a");
        let mut inserted = state.clone();
        inserted.cards.insert(indices.1, added.clone());
        let restored = scope_descriptor(&inserted, roots[0], &roots, Some(added.id));
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
}
