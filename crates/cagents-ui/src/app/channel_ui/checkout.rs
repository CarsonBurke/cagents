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
    /// linked worktree's canonical path. A project whose worktrees have not
    /// loaded yet still knows its main checkout, so opening it before then
    /// does not leave a pane in its strip that would keep it out of channels.
    ///
    /// The checkout is resolved as the daemon resolves a channel's, so the
    /// answer is the form the registry records and every window compares: a
    /// submodule's main checkout, which the rows cannot tell from a linked
    /// worktree, is still the main checkout, and a worktree reached through a
    /// symlink is still the one worktree. Opening a checkout is a user's
    /// action, so the few small reads this takes stay out of the view.
    fn checkout_at(&self, root: &Path) -> Option<(ProjectId, Option<PathBuf>)> {
        use super::super::worktree_ui::project_main_checkout;
        let (project, main) = match self.sidebar_model.worktree_at(root) {
            Some(key) => {
                let project = self.sidebar_model.project(key.project_id)?;
                (project, project_main_checkout(project))
            }
            None => self.sidebar_model.projects.iter().find_map(|project| {
                let main = project_main_checkout(project);
                (main == root).then_some((project, main))
            })?,
        };
        // A Git directory kept elsewhere, as a submodule's is, names none of
        // its rows, so which row is its main checkout rests on their order,
        // which only the Git listing guarantees. There the files decide.
        let separate = project
            .repository
            .file_name()
            .is_some_and(|name| name != ".git");
        if project.kind == ProjectKind::Folder || (main == root && !separate) {
            return Some((project.id, None));
        }
        // A checkout the daemon would refuse, such as one removed since the
        // rows were listed, is offered as listed and refused there.
        let repository = if crate::device::is_remote() {
            project.repository.clone()
        } else {
            std::fs::canonicalize(&project.repository)
                .unwrap_or_else(|_| project.repository.clone())
        };
        let checkout = crate::workspace_model::canonical_checkout(&repository, root)
            .unwrap_or_else(|_| Some(root.to_path_buf()));
        Some((project.id, checkout))
    }

    /// Moves the panes this window alone kept in a worktree strip into their
    /// checkout's channel, once it holds the daemon's model.
    ///
    /// The daemon moved every strip pane it knew into a channel when it
    /// started. A pane only this window had, never published, is still in
    /// its strip here; left there, it would keep the strip alive and the
    /// daemon refuses it. It joins the channel the daemon would have chosen:
    /// a worktree's oldest channel, the main checkout's oldest one named
    /// after it, or else a new one named after the branch.
    pub(in crate::app) fn adopt_strip_cards(&mut self) {
        if !std::mem::take(&mut self.channel_ui.strip_adoption_pending)
            || self.workspace_model.identity == crate::workspace_model::NO_REGISTRY
        {
            return;
        }
        let shared: HashSet<CardId> = self
            .workspace_model
            .views
            .iter()
            .flat_map(|view| view.panes.iter().map(|pane| pane.id))
            .collect();
        let channeled: HashSet<CardId> = self
            .durable
            .channels
            .iter()
            .flat_map(|channel| channel.cards.iter().copied())
            .collect();
        let roots = &self.worktree_paths;
        let mut strips: std::collections::BTreeMap<PathBuf, Vec<CardId>> = Default::default();
        for card in self.durable.cards.iter() {
            if !card.existence.is_open()
                || card.kind.is_utility()
                || channeled.contains(&card.id)
                || shared.contains(&card.id)
                || self.shared_views.quarantined.contains(&card.id)
            {
                continue;
            }
            if let Some(root) = self.unchanneled_card_worktree_indexed(card, &roots) {
                strips.entry(root.to_path_buf()).or_default().push(card.id);
            }
        }
        let mut adopted = false;
        for (root, cards) in strips {
            let Some((project_id, checkout)) = self.checkout_at(&root) else {
                // A project whose rows have not loaded yet is tried again when
                // they do.
                self.channel_ui.strip_adoption_pending = true;
                continue;
            };
            let title = self.checkout_title(project_id, &root);
            let existing = match checkout.as_deref() {
                Some(checkout) => self.channel_for_checkout(project_id, Some(checkout)),
                None => self.main_checkout_channel(project_id, &title),
            };
            let id = match existing {
                Some(id) => id,
                None => {
                    match self.create_channel(
                        project_id,
                        title,
                        crate::model::ChannelTitleSource::Manual,
                        checkout,
                    ) {
                        Some(id) => id,
                        None => continue,
                    }
                }
            };
            tracing::info!(?root, panes = cards.len(), %id, "moved a strip's panes into its channel");
            if let Some(channel) = Arc::make_mut(&mut self.durable)
                .channels
                .iter_mut()
                .find(|channel| channel.id == id)
            {
                channel.cards.extend(cards);
                adopted = true;
            }
        }
        if adopted {
            self.refresh_sidebar_channels();
            self.persist();
            self.publish_view_changes();
        }
    }

    /// The channel the daemon's startup repair moved the main checkout's
    /// strip into: the oldest channel of no checkout named after it by hand,
    /// as the daemon chooses (`checkout_channel` in the registry). Panes this
    /// window alone kept join it rather than a second channel of that name.
    fn main_checkout_channel(&self, project_id: ProjectId, title: &str) -> Option<ChannelId> {
        let title =
            crate::workspace_model::channel_title(title).unwrap_or_else(|| title.to_owned());
        self.durable
            .channels
            .iter()
            .filter(|channel| {
                channel.project_id == project_id
                    && channel.checkout.is_none()
                    && channel.title == title
                    && channel.title_source == crate::model::ChannelTitleSource::Manual
                    && !self.channel_ui.is_deleting(channel.id)
                    && !self.channel_retiring(channel.id)
            })
            .min_by_key(|channel| (channel.ordinal, channel.id))
            .map(|channel| channel.id)
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
    fn a_worktree_listed_through_a_symlink_opens_the_channel_the_registry_recorded() {
        let directory = tempfile::tempdir().unwrap();
        let real = std::fs::canonicalize(directory.path()).unwrap();
        let repo = real.join("repo");
        std::fs::create_dir(&repo).unwrap();
        let git = |arguments: &[&str]| {
            let status = std::process::Command::new("git")
                .args(arguments)
                .current_dir(&repo)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .env("GIT_AUTHOR_NAME", "Cagents")
                .env("GIT_AUTHOR_EMAIL", "cagents@example.invalid")
                .env("GIT_COMMITTER_NAME", "Cagents")
                .env("GIT_COMMITTER_EMAIL", "cagents@example.invalid")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .expect("git runs");
            assert!(status.success(), "git {arguments:?}");
        };
        git(&["init", "--quiet"]);
        git(&["commit", "--quiet", "--allow-empty", "-m", "start"]);
        git(&["worktree", "add", "--quiet", "-b", "feature", "../feature"]);
        // The rows name both checkouts through a symlinked parent, while the
        // registry recorded the worktree's canonical path.
        let alias = real.join("alias");
        std::os::unix::fs::symlink(&real, &alias).unwrap();
        let primary = SidebarWorktree::from_durable(
            WorktreeId::new(),
            alias.join("repo"),
            Some("main".into()),
        );
        let feature = SidebarWorktree::from_durable(
            WorktreeId::new(),
            alias.join("feature"),
            Some("feature".into()),
        );
        let (mut app, _state, project) = super::super::super::project_ui::tests::deletable_app(
            &alias.join("repo/.git"),
            &feature,
            &primary,
        );
        let recorded = ChannelId::new();
        Arc::make_mut(&mut app.durable)
            .channels
            .push(ProjectChannel {
                title_source: crate::model::ChannelTitleSource::Manual,
                id: recorded,
                project_id: project,
                title: "feature".into(),
                ordinal: 1,
                shared: true,
                cards: Vec::new(),
                focused: None,
                checkout: Some(real.join("feature")),
            });
        app.refresh_sidebar_channels();

        drop(app.update(Message::Sidebar(SidebarAction::OpenWorktree {
            project_id: project,
            worktree_id: feature.id,
            path: feature.path.clone(),
        })));

        assert_eq!(
            app.durable.channels.len(),
            1,
            "no second channel for one worktree"
        );
        assert_eq!(app.durable.active_channel, Some(recorded));
        assert_eq!(
            app.terminal_directory(),
            Some(feature.path.clone()),
            "its panes start where the row says the worktree is"
        );
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

    /// A pane in the worktree's strip, as an older window filed it there.
    fn strip_pane(directory: &Path) -> CardState {
        let mut pane = super::super::super::tests::terminal_card_state();
        let CardKind::Terminal(session) = &mut pane.kind else {
            unreachable!();
        };
        session.launch.working_directory = directory.to_path_buf();
        pane
    }

    impl Fixture {
        /// The window holds the daemon's model, so adoption may run.
        fn connect(&mut self) {
            self.app.workspace_model.identity = uuid::Uuid::new_v4();
            self.app.channel_ui.strip_adoption_pending = true;
        }

        fn cards(&self, id: ChannelId) -> Vec<CardId> {
            self.app
                .durable
                .channels
                .iter()
                .find(|channel| channel.id == id)
                .unwrap()
                .cards
                .clone()
        }
    }

    /// A strip pane only this window had joins its worktree's oldest channel,
    /// so the strip no longer holds panes and the worktree opens there.
    #[test]
    fn a_strip_pane_only_this_window_has_joins_its_worktrees_channel() {
        let mut fixture = fixture();
        let linked = fixture.linked.path().to_path_buf();
        let pane = strip_pane(&linked);
        Arc::make_mut(&mut fixture.app.durable).cards = vec![pane.clone()].into();
        let older = fixture.channel(1, Some(&linked));
        let newer = fixture.channel(2, Some(&linked));
        fixture.connect();

        fixture.app.adopt_strip_cards();

        assert_eq!(fixture.cards(older), vec![pane.id]);
        assert!(fixture.cards(newer).is_empty());
        assert!(!fixture.app.legacy_strip_holds_panes(&linked));
        fixture.open_worktree();
        assert_eq!(fixture.app.durable.active_channel, Some(older));
    }

    /// A worktree with no channel gets one named after its branch.
    #[test]
    fn a_strip_pane_without_a_channel_gets_one_named_for_its_branch() {
        let mut fixture = fixture();
        let linked = fixture.linked.path().to_path_buf();
        let pane = strip_pane(&linked);
        Arc::make_mut(&mut fixture.app.durable).cards = vec![pane.clone()].into();
        fixture.connect();

        fixture.app.adopt_strip_cards();

        let channel = &fixture.app.durable.channels[0];
        assert_eq!(channel.title, "feature");
        assert_eq!(channel.cards, vec![pane.id]);
        assert!(channel.checkout.is_some());
    }

    /// A main-checkout strip pane only this window had joins the channel the
    /// daemon's repair made for that strip, named after the checkout, rather
    /// than a second channel of that name or an unrelated conversation.
    #[test]
    fn a_main_checkout_strip_pane_joins_the_channel_named_after_it() {
        let mut fixture = fixture();
        let main = fixture.main.path().to_path_buf();
        let pane = strip_pane(&main);
        Arc::make_mut(&mut fixture.app.durable).cards = vec![pane.clone()].into();
        let conversation = fixture.channel(1, None);
        let repaired = fixture.channel(2, None);
        Arc::make_mut(&mut fixture.app.durable)
            .channels
            .iter_mut()
            .find(|channel| channel.id == repaired)
            .unwrap()
            .title = "main".into();
        fixture.connect();

        fixture.app.adopt_strip_cards();

        assert_eq!(fixture.app.durable.channels.len(), 2);
        assert_eq!(fixture.cards(repaired), vec![pane.id]);
        assert!(fixture.cards(conversation).is_empty());
    }

    /// Before the window holds the daemon's model it cannot tell its own
    /// panes from the daemon's, so it moves nothing; a pane the daemon holds
    /// is the daemon's to move.
    #[test]
    fn strip_panes_wait_for_the_model_and_leave_the_daemons_panes_alone() {
        let mut fixture = fixture();
        let linked = fixture.linked.path().to_path_buf();
        let pane = strip_pane(&linked);
        Arc::make_mut(&mut fixture.app.durable).cards = vec![pane.clone()].into();
        let channel = fixture.channel(1, Some(&linked));
        fixture.app.channel_ui.strip_adoption_pending = true;

        fixture.app.adopt_strip_cards();
        assert!(fixture.cards(channel).is_empty(), "no model yet");

        fixture.connect();
        fixture
            .app
            .workspace_model
            .views
            .push(crate::workspace_model::views::View {
                scope: crate::workspace_model::views::Scope::Channel(ChannelId::new()),
                revision: 1,
                panes: vec![crate::workspace_model::views::Pane::from_card(&pane, None).unwrap()],
                retired: vec![],
            });
        fixture.app.adopt_strip_cards();
        assert!(fixture.cards(channel).is_empty(), "the daemon holds it");
    }

    /// A channel whose worktree is no longer listed says so and offers to
    /// make it again where it was, on the branch this window last saw there.
    /// The channel's own name is not taken for the branch.
    #[test]
    fn a_channel_whose_worktree_is_gone_offers_to_recreate_it() {
        let mut fixture = fixture();
        let linked = fixture.linked.path().to_path_buf();
        let present = fixture.channel(1, Some(&linked));
        let gone = std::path::PathBuf::from("/nonexistent/feature-two");
        let missing = fixture.channel(2, Some(&gone));
        let main = fixture.channel(3, None);

        assert_eq!(fixture.app.missing_checkout(present), None);
        assert_eq!(fixture.app.missing_checkout(main), None);
        let found = fixture.app.missing_checkout(missing).expect("missing");
        assert_eq!(found.path, gone);
        assert_eq!(found.branch, None, "no listing ever named its branch");
        drop(fixture.app.update(Message::MissingCheckout(
            super::super::missing_checkout::Action::Recreate(missing),
        )));
        let dialog = fixture.app.worktree_dialog.as_ref().expect("the dialog");
        assert_eq!(dialog.draft.branch, "", "an unknown branch is asked for");
        assert_eq!(dialog.draft.destination, gone.display().to_string());
        fixture.app.worktree_dialog = None;

        fixture
            .app
            .channel_ui
            .known_branches
            .insert(gone.clone(), "feat/two".into());
        drop(fixture.app.update(Message::MissingCheckout(
            super::super::missing_checkout::Action::Recreate(missing),
        )));
        let dialog = fixture.app.worktree_dialog.as_ref().expect("the dialog");
        assert_eq!(dialog.draft.branch, "feat/two");
    }

    /// A listing records each worktree's branch, so it outlives the worktree.
    #[test]
    fn a_listing_remembers_each_worktrees_branch() {
        let mut fixture = fixture();
        let linked = fixture.linked.path().to_path_buf();
        let project = fixture.project;
        fixture.app.remember_worktree_branches(project);
        assert_eq!(
            fixture
                .app
                .channel_ui
                .known_branches
                .get(&linked)
                .map(String::as_str),
            Some("feature")
        );
    }
}
