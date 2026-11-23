//! Opening a checkout means working in a channel there. Every way of going to
//! a project or one of its worktrees, from the sidebar, the Worktrees view, a
//! newly created worktree or a resumed conversation, lands in the channel this
//! module picks for that checkout, so one checkout does not split its work
//! between a strip and several channels depending on where it was opened.
use super::*;
use crate::model::WorkspaceDestination;

impl Cagents {
    /// The channel that opening `checkout` of project `project_id` enters,
    /// where `None` is the project's main checkout.
    ///
    /// A linked worktree goes to its oldest channel, so that every route to
    /// it agrees on one. The main checkout is where most channels work, so it
    /// goes to the one the user was last in, falling back to the oldest when
    /// this window has visited none of them. A channel being deleted or
    /// retired is no longer a place to work and never counts.
    pub(in crate::app) fn channel_for_checkout(
        &self,
        project_id: ProjectId,
        checkout: Option<&Path>,
    ) -> Option<ChannelId> {
        let candidate = |id: ChannelId| {
            self.durable.channels.iter().any(|channel| {
                channel.id == id
                    && channel.project_id == project_id
                    && channel.checkout.as_deref() == checkout
                    && !self.channel_ui.is_deleting(id)
                    && !self.channel_retiring(id)
            })
        };
        let recent = || {
            self.durable.active_channel.into_iter().chain(
                self.workspace_history.recent().iter().filter_map(
                    |destination| match destination {
                        WorkspaceDestination::Channel(id) => Some(*id),
                        WorkspaceDestination::Folder(_) | WorkspaceDestination::Worktree(_) => None,
                    },
                ),
            )
        };
        if checkout.is_none()
            && let Some(id) = recent().find(|&id| candidate(id))
        {
            return Some(id);
        }
        self.durable
            .channels
            .iter()
            .filter(|channel| candidate(channel.id))
            .min_by_key(|channel| (channel.ordinal, channel.id))
            .map(|channel| channel.id)
    }

    /// Opens the checkout holding `path` in its channel, creating one named
    /// after its branch, or else its directory, when it has none. With
    /// `default_pane`, an empty channel gets the pane a new one starts with.
    ///
    /// A checkout whose strip still holds panes outside every channel, or a
    /// directory no project lists, opens as that strip instead: those panes
    /// would otherwise have nowhere to be reached. The answer reports whether
    /// the window now shows the checkout, so that a caller adds panes only
    /// where the user asked for them.
    pub(in crate::app) fn open_checkout(
        &mut self,
        path: PathBuf,
        default_pane: bool,
        change: DurableChange,
    ) -> (bool, Task<Message>) {
        let root = self
            .refresh_worktree_owners(Some(&path))
            .expect("a selected directory resolves to a root");
        let Some((project_id, checkout)) = self.checkout_at(&root) else {
            return self.show_worktree(path, default_pane, change);
        };
        if self.legacy_strip_holds_panes(&root) {
            return self.show_worktree(path, default_pane, change);
        }
        // A worktree being removed is refused here as the strip refuses it,
        // before an existing channel would send new panes into it.
        if let Err(error) = self.worktree_protections.protect_next(&root) {
            self.status = Some(TransientNotice::warning(format!(
                "Cannot open this worktree yet: {error}"
            )));
            return (false, Task::none());
        }
        let (id, created) = match self.channel_for_checkout(project_id, checkout.as_deref()) {
            Some(id) => (id, false),
            None => {
                let title = self.checkout_title(project_id, &root);
                let Some(id) = self.create_channel(
                    project_id,
                    title,
                    crate::model::ChannelTitleSource::Manual,
                    checkout,
                ) else {
                    return (false, Task::none());
                };
                (id, true)
            }
        };
        let scope = if default_pane {
            self.show_channel_with_default(id, created, change)
        } else {
            self.show_channel(id, change)
        };
        if created {
            self.persist();
        }
        (self.durable.active_channel == Some(id), scope)
    }

    /// The project listing the checkout at `root`, and which of its checkouts
    /// that is: `None` for the main checkout or a folder, otherwise the
    /// linked worktree's path. A project whose worktrees have not loaded yet
    /// still knows its main checkout, so opening it before then does not
    /// leave a pane in its strip that would keep it out of channels.
    fn checkout_at(&self, root: &Path) -> Option<(ProjectId, Option<PathBuf>)> {
        use super::super::worktree_ui::project_main_checkout;
        if let Some(key) = self.sidebar_model.worktree_at(root) {
            let project = self.sidebar_model.project(key.project_id)?;
            let main = project_main_checkout(project);
            return Some((
                key.project_id,
                (key.canonical_path != main).then_some(key.canonical_path),
            ));
        }
        self.sidebar_model
            .projects
            .iter()
            .find(|project| project_main_checkout(project) == root)
            .map(|project| (project.id, None))
    }

    /// A new channel's name for the checkout at `root`: its branch, or the
    /// directory's name for a detached checkout or a folder.
    fn checkout_title(&self, project_id: ProjectId, root: &Path) -> String {
        self.sidebar_model
            .project(project_id)
            .and_then(|project| {
                project
                    .worktrees
                    .iter()
                    .find(|worktree| worktree.path == root)
            })
            .and_then(|worktree| worktree.branch.clone())
            .or_else(|| {
                root.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            })
            .unwrap_or_else(|| root.display().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::WorktreeId;
    use crate::sidebar::SidebarWorktree;

    /// A project whose main checkout and one linked worktree, on branch
    /// `feature`, are real directories, with no pane open anywhere.
    struct Fixture {
        app: Cagents,
        project: ProjectId,
        main: tempfile::TempDir,
        linked: tempfile::TempDir,
        linked_id: WorktreeId,
        _state: tempfile::TempDir,
    }

    fn fixture() -> Fixture {
        let main = tempfile::tempdir().unwrap();
        let linked = tempfile::tempdir().unwrap();
        let primary = SidebarWorktree::from_durable(
            WorktreeId::new(),
            main.path().into(),
            Some("main".into()),
        );
        let feature = SidebarWorktree::from_durable(
            WorktreeId::new(),
            linked.path().into(),
            Some("feature".into()),
        );
        let (app, state, project) =
            super::super::super::project_ui::tests::deletable_app(main.path(), &feature, &primary);
        Fixture {
            app,
            project,
            main,
            linked,
            linked_id: feature.id,
            _state: state,
        }
    }

    impl Fixture {
        /// Adds a named, shared channel of this project.
        fn channel(&mut self, ordinal: u32, checkout: Option<&Path>) -> ChannelId {
            let id = ChannelId::new();
            Arc::make_mut(&mut self.app.durable)
                .channels
                .push(ProjectChannel {
                    title_source: crate::model::ChannelTitleSource::Manual,
                    id,
                    project_id: self.project,
                    title: format!("Channel {ordinal}"),
                    ordinal,
                    shared: true,
                    cards: Vec::new(),
                    focused: None,
                    checkout: checkout.map(Path::to_path_buf),
                });
            self.app.refresh_sidebar_channels();
            id
        }

        fn open_worktree(&mut self) {
            drop(
                self.app
                    .update(Message::Sidebar(SidebarAction::OpenWorktree {
                        project_id: self.project,
                        worktree_id: self.linked_id,
                        path: self.linked.path().into(),
                    })),
            );
        }

        fn open_project(&mut self) {
            let repository = self.main.path().into();
            drop(
                self.app
                    .update(Message::Sidebar(SidebarAction::OpenRepository {
                        project_id: self.project,
                        repository,
                    })),
            );
        }

        fn channels(&self) -> Vec<(String, Option<PathBuf>)> {
            self.app
                .durable
                .channels
                .iter()
                .map(|channel| (channel.title.clone(), channel.checkout.clone()))
                .collect()
        }
    }

    #[test]
    fn a_linked_worktree_opens_in_its_oldest_channel_and_starts_terminals_there() {
        let mut fixture = fixture();
        let linked = fixture.linked.path().to_path_buf();
        fixture.channel(1, None);
        fixture.channel(5, Some(&linked));
        let oldest = fixture.channel(3, Some(&linked));

        fixture.open_worktree();

        assert_eq!(fixture.app.durable.active_channel, Some(oldest));
        assert_eq!(fixture.app.terminal_directory(), Some(linked.clone()));
        let cards = fixture.app.durable.cards.len();
        drop(
            fixture
                .app
                .handle_project_action(super::super::super::project_ui::Action::Command {
                    project_id: fixture.project,
                    worktree_id: Some(fixture.linked_id),
                    command: crate::sidebar::ProjectCommand::NewTerminal,
                }),
        );
        assert_eq!(fixture.app.durable.active_channel, Some(oldest));
        assert_eq!(fixture.app.durable.cards.len(), cards + 1);
        let terminal = fixture.app.durable.cards.last().unwrap();
        let CardKind::Terminal(session) = &terminal.kind else {
            panic!("a terminal");
        };
        assert_eq!(session.launch.working_directory, linked);
        assert!(
            fixture
                .app
                .durable
                .channels
                .iter()
                .any(|channel| channel.id == oldest && channel.cards.contains(&terminal.id))
        );
    }

    #[test]
    fn the_main_checkout_opens_the_channel_used_last_else_the_oldest() {
        let mut fixture = fixture();
        let linked = fixture.linked.path().to_path_buf();
        let oldest = fixture.channel(1, None);
        let recent = fixture.channel(2, None);
        let feature = fixture.channel(3, Some(&linked));

        fixture.open_project();
        assert_eq!(
            fixture.app.durable.active_channel,
            Some(oldest),
            "with no visit to go by, the oldest channel"
        );

        drop(fixture.app.select_channel(feature));
        fixture.app.workspace_history = crate::tab_history::TabHistory::from_recent([
            WorkspaceDestination::Channel(feature),
            WorkspaceDestination::Channel(recent),
            WorkspaceDestination::Channel(oldest),
        ]);
        fixture.open_project();
        assert_eq!(fixture.app.durable.active_channel, Some(recent));
        assert_eq!(
            fixture.app.terminal_directory().as_deref(),
            Some(fixture.main.path())
        );
    }

    #[test]
    fn a_checkout_without_a_channel_gets_one_named_for_its_branch() {
        let mut fixture = fixture();
        let linked = fixture.linked.path().to_path_buf();

        fixture.open_worktree();

        assert_eq!(
            fixture.channels(),
            vec![("feature".to_owned(), Some(linked.clone()))]
        );
        let created = fixture.app.durable.channels[0].id;
        assert_eq!(fixture.app.durable.active_channel, Some(created));
        assert_eq!(fixture.app.active_workspace, Some(linked));
        assert!(
            fixture.app.channel_is_named(created),
            "a channel made for a checkout stays when left empty"
        );

        fixture.open_project();
        assert_eq!(
            fixture.channels(),
            vec![
                ("feature".to_owned(), Some(fixture.linked.path().into())),
                ("main".to_owned(), None),
            ]
        );
        fixture.open_worktree();
        assert_eq!(fixture.app.durable.active_channel, Some(created));
        assert_eq!(fixture.app.durable.channels.len(), 2);
    }

    #[test]
    fn a_checkout_whose_strip_holds_panes_still_opens_as_the_strip() {
        let mut fixture = fixture();
        let linked = fixture.linked.path().to_path_buf();
        let mut pane = super::super::super::tests::terminal_card_state();
        let CardKind::Terminal(session) = &mut pane.kind else {
            unreachable!();
        };
        session.launch.working_directory = linked.clone();
        Arc::make_mut(&mut fixture.app.durable).cards = vec![pane.clone()].into();
        fixture.channel(1, Some(&linked));

        fixture.open_worktree();

        assert_eq!(fixture.app.durable.active_channel, None);
        assert_eq!(fixture.app.active_workspace, Some(linked));
        assert!(fixture.app.workspace.order().any(|card| card == pane.id));
    }

    #[test]
    fn a_project_opened_before_its_worktrees_load_opens_its_channel() {
        let mut app = super::super::super::tests::context_inspector_test_app(false);
        let main = tempfile::tempdir().unwrap();
        let repository = main.path().join(".git");
        std::fs::create_dir(&repository).unwrap();
        let project = ProjectId::new();
        app.sidebar_model.projects.push(SidebarProject::new(
            project,
            RepositoryId::new(),
            "repo".into(),
            repository.clone(),
            Vec::new(),
            Vec::new(),
        ));

        drop(app.update(Message::Sidebar(SidebarAction::OpenRepository {
            project_id: project,
            repository,
        })));

        let [channel] = &app.durable.channels[..] else {
            panic!("the main checkout's channel");
        };
        assert_eq!(channel.checkout, None);
        assert_eq!(app.durable.active_channel, Some(channel.id));
        assert_eq!(app.active_workspace.as_deref(), Some(main.path()));
    }

    #[test]
    fn a_folder_opens_in_its_own_channel() {
        let mut app = super::super::super::tests::context_inspector_test_app(false);
        let folder = tempfile::tempdir().unwrap();
        let project = ProjectId::new();
        let mut row = SidebarProject::new(
            project,
            RepositoryId::new(),
            "notes".into(),
            folder.path().into(),
            Vec::new(),
            Vec::new(),
        );
        row.kind = ProjectKind::Folder;
        app.sidebar_model.projects.push(row);

        let open = || {
            Message::Sidebar(SidebarAction::OpenRepository {
                project_id: project,
                repository: folder.path().into(),
            })
        };
        drop(app.update(open()));

        let [channel] = &app.durable.channels[..] else {
            panic!("the folder's channel");
        };
        let channel = channel.clone();
        assert_eq!(channel.project_id, project);
        assert_eq!(channel.checkout, None);
        assert_eq!(
            channel.title,
            folder.path().file_name().unwrap().to_string_lossy()
        );
        assert_eq!(app.durable.active_channel, Some(channel.id));
        assert_eq!(app.active_workspace.as_deref(), Some(folder.path()));

        drop(app.update(open()));
        assert_eq!(app.durable.channels.len(), 1, "opening again reuses it");
    }
}
