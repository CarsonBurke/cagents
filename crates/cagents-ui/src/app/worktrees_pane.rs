//! The Work pane's Worktrees view, as the application drives it: opening it,
//! deriving who works in each checkout, and carrying out its actions through
//! the sidebar's own handlers.
use super::worktree_ui::{ResolveRoot, card_directory};
use super::*;
use crate::worktrees_ui::{Action, Agent, AgentState, Agents, Effect};

impl Cagents {
    /// Shows the Work pane on its checkouts, where the user is.
    pub(super) fn open_worktrees(&mut self) -> Task<Message> {
        self.work_ui.show(true);
        self.open_utility_card(CardKind::Work)
    }

    pub(super) fn update_worktrees_ui(&mut self, action: Action) -> Task<Message> {
        let Some(effect) = self.worktrees_ui.update(action) else {
            return Task::none();
        };
        match effect {
            Effect::Layout(layout) => self.update_work_ui(crate::work_ui::Action::Layout(layout)),
            Effect::Sidebar(action) => self.dispatch(Message::Sidebar(action)),
            Effect::FocusPane(card) => self.focus_attention_card(card),
        }
    }

    /// Whether this window has the Work pane open on its checkouts.
    pub(super) fn worktrees_pane_open(&self) -> bool {
        self.work_ui.shows_worktrees()
            && self
                .durable
                .cards
                .iter()
                .any(|card| card.kind == CardKind::Work && card.existence.is_open())
    }

    /// Lists the checkouts again when the projects changed, and who works in
    /// each, while the view is open. A closed view costs nothing.
    pub(super) fn refresh_worktrees_pane(&mut self) {
        if !self.worktrees_pane_open() {
            return;
        }
        let revision = sidebar::Revisioned::revision(&self.sidebar_model.projects);
        self.worktrees_ui
            .refresh(revision, &self.sidebar_model.projects);
        let agents = self.worktree_agents();
        self.worktrees_ui.set_agents(agents);
    }

    /// Carries a pane's new title to its chips, for a projection that moved
    /// only titles.
    pub(super) fn retitle_worktrees_pane(&mut self) {
        if !self.worktrees_pane_open() {
            return;
        }
        let mut state = std::mem::take(&mut self.worktrees_ui);
        state.retitle(|card| {
            self.card_state(card)
                .map(|card| self.card_display_title(card).to_owned())
        });
        self.worktrees_ui = state;
    }

    /// Every open chat and terminal, under each checkout row its directory is
    /// in, working agents first. A pane in a channel counts as much as one
    /// outside every channel: the view answers who works in a checkout, not
    /// where their pane is filed.
    fn worktree_agents(&self) -> Agents {
        let index = &self.worktree_paths;
        let mut channels = HashMap::new();
        for channel in &self.durable.channels {
            for card in &channel.cards {
                channels.entry(*card).or_insert(channel.id);
            }
        }
        let titles = self
            .sidebar_model
            .projects
            .iter()
            .flat_map(|project| &project.channels)
            .map(|channel| (channel.id, channel.title.as_str()))
            .collect::<HashMap<_, _>>();
        let mut agents = Agents::new();
        for card in self
            .durable
            .cards
            .iter()
            .filter(|card| card.existence.is_open())
        {
            let activity = self.sidebar_state.card_activity(card.id);
            let (provider, state) = match (&card.kind, activity) {
                (CardKind::Chat(_) | CardKind::Terminal(_), Some(activity)) => (
                    Some(activity.provider),
                    match activity.state {
                        crate::agent::ActivityState::Working => AgentState::Working,
                        crate::agent::ActivityState::Waiting => AgentState::Waiting,
                        crate::agent::ActivityState::Idle
                        | crate::agent::ActivityState::Unknown => AgentState::Idle,
                    },
                ),
                (CardKind::Chat(session), None) => {
                    (Some(session.launch.provider), AgentState::Idle)
                }
                // A shell that ended, or whose runtime failed, has nobody in
                // it.
                (CardKind::Terminal(session), None)
                    if !terminal_ended(session)
                        && !matches!(session.continuity, TerminalContinuity::Failed { .. }) =>
                {
                    match crate::agent::Provider::recognize_launch(&session.launch) {
                        Some(provider) => (Some(provider), AgentState::Starting),
                        None => (None, AgentState::Shell),
                    }
                }
                _ => continue,
            };
            // Where the sidebar files the pane: its matched owner or its
            // explicit scope first, so a pane reached through a symlink or
            // scoped to a checkout lists there too, then its directory.
            let root = match self
                .worktree_owners
                .get(&card.id)
                .or_else(|| self.durable.card_scopes.get(&card.id))
            {
                Some(scope) => Some(index.resolve(scope).unwrap_or(scope.as_path())),
                None => card_directory(card).and_then(|directory| index.resolve(directory)),
            };
            let Some(root) = root else {
                continue;
            };
            let rows = index.rows(root);
            if rows.is_empty() {
                continue;
            }
            let agent = Agent {
                card: card.id,
                title: self.card_display_title(card).to_owned(),
                provider,
                state,
                channel: channels
                    .get(&card.id)
                    .and_then(|channel| titles.get(channel))
                    .map(|title| (*title).to_owned()),
            };
            for row in rows {
                agents.entry(*row).or_default().push(agent.clone());
            }
        }
        for working in agents.values_mut() {
            working.sort_by_key(|agent| match agent.state {
                AgentState::Working => 0,
                AgentState::Waiting => 1,
                AgentState::Starting => 2,
                AgentState::Idle => 3,
                AgentState::Shell => 4,
            });
        }
        agents
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ChannelId, ProjectChannel, ProjectId, WorktreeId};
    use crate::repository::RepositoryId;
    use crate::sidebar::{SidebarProject, SidebarWorktree};

    fn work_cards(app: &Cagents) -> Vec<CardId> {
        app.durable
            .cards
            .iter()
            .filter(|card| card.kind == CardKind::Work && card.existence.is_open())
            .map(|card| card.id)
            .collect()
    }

    #[test]
    fn the_worktrees_view_is_the_work_pane_named_for_what_it_shows() {
        let mut app = crate::app::tests::context_inspector_test_app(true);
        drop(app.update(Message::RunCommand(
            workspace_widget::GlobalShortcut::OpenWorktrees,
        )));
        let [pane] = work_cards(&app)[..] else {
            panic!("one Work pane opened");
        };
        assert_eq!(app.workspace.focused(), Some(pane));
        assert!(app.work_ui.shows_worktrees());
        let card = app.card_state(pane).unwrap();
        assert_eq!(app.card_display_title(card), "Worktrees");

        // The sidebar's Work board shows the same pane on its board.
        drop(app.update(Message::Sidebar(SidebarAction::OpenWork)));
        assert_eq!(work_cards(&app), [pane], "still one pane");
        assert!(!app.work_ui.shows_worktrees());
        let card = app.card_state(pane).unwrap();
        assert_eq!(app.card_display_title(card), "Work");

        // And the palette's command shows the checkouts again.
        drop(app.open_worktrees());
        assert_eq!(work_cards(&app), [pane]);
        assert!(app.work_ui.shows_worktrees());
    }

    #[test]
    fn a_checkout_lists_the_panes_working_in_it_with_their_channel() {
        let mut app = crate::app::tests::context_inspector_test_app(true);
        let project = ProjectId::new();
        let main = WorktreeId::new();
        let feature = WorktreeId::new();
        app.sidebar_model.projects.push(SidebarProject::new(
            project,
            RepositoryId::new(),
            "repo".into(),
            "/repo".into(),
            vec![
                SidebarWorktree::from_durable(main, "/repo".into(), Some("main".into())),
                SidebarWorktree::from_durable(
                    feature,
                    "/repo-feature".into(),
                    Some("feature".into()),
                ),
            ],
            vec![],
        ));
        // A shell in a subdirectory of the feature worktree, filed in a
        // channel.
        let shell = app.durable.cards[0].id;
        if let CardKind::Terminal(session) = &mut app.card_state_mut(shell).unwrap().kind {
            session.launch.working_directory = "/repo-feature/src".into();
        }
        let channel = ChannelId::new();
        Arc::make_mut(&mut app.durable)
            .channels
            .push(ProjectChannel {
                id: channel,
                project_id: project,
                title: "release".into(),
                title_source: Default::default(),
                ordinal: 0,
                shared: false,
                cards: vec![shell],
                focused: Some(shell),
            });

        // Nothing is derived while the view is closed.
        drop(app.project_workspace(false));
        assert!(app.worktrees_ui.agents().is_empty());

        drop(app.open_worktrees());
        drop(app.project_workspace(false));
        let agents = app.worktrees_ui.agents().clone();
        let listed = &agents[&(project, feature)];
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].card, shell);
        assert_eq!(listed[0].state, AgentState::Shell);
        assert_eq!(listed[0].channel.as_deref(), Some("release"));
        assert!(!agents.contains_key(&(project, main)));

        // A pane scoped to a checkout lists there, as the sidebar files it,
        // whatever directory it started in.
        Arc::make_mut(&mut app.durable)
            .card_scopes
            .insert(shell, "/repo".into());
        drop(app.project_workspace(false));
        let agents = app.worktrees_ui.agents();
        assert_eq!(agents[&(project, main)][0].card, shell);
        assert!(!agents.contains_key(&(project, feature)));

        // Pressing it goes to the pane.
        drop(app.update(Message::WorktreesUi(Action::FocusPane(shell))));
        assert_eq!(app.workspace.focused(), Some(shell));
    }

    #[test]
    fn a_dialog_refuses_the_views_actions_but_not_closing_its_menu() {
        assert!(super::super::user_workspace_message_blocked_while_modal(
            &Message::WorktreesUi(Action::Query("x".into()))
        ));
        assert!(!super::super::user_workspace_message_blocked_while_modal(
            &Message::WorktreesUi(Action::Menu(None))
        ));
    }
}
