//! The inspector's Work and Mentioned sections: the pull requests a chat
//! pane's conversation worked on and the worktrees it edited, and, when the
//! settings ask, the pull requests it only mentioned. The daemon keeps the
//! list and each pull request's state, changes and checks current while the
//! pane's inspector is shown. Choosing a row binds the inspector's changes
//! and files to that work.

use super::*;

mod conversation;

pub(super) use conversation::WorkKey;
// The inspected conversation's chats label their links from the same list.
pub(super) use conversation::Watch as ConversationWatch;

#[derive(Debug, Clone)]
pub enum Action {
    /// A shown inspector's conversation's pull requests changed.
    Conversation(conversation::Watch),
    /// Open a listed pull request, in the review pane or on GitHub as the
    /// settings choose.
    Open {
        repository: GitHubRepoKey,
        number: u64,
        url: Url,
    },
    /// Show this work's changes and files in `card`'s inspector.
    Choose { card: CardId, key: WorkKey },
    /// Open one check's details page.
    OpenCheck(Url),
    /// Open a listed pull request in the browser, from its row's button.
    OpenInBrowser(Url),
}

impl Action {
    pub(super) fn allowed_while_modal(&self) -> bool {
        matches!(self, Self::Conversation(..))
    }
}

/// What the inspector's changes and files show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum WorkScope {
    /// The worktree the pane's card names, as without a conversation.
    Focus,
    /// The worktree of the work shown, named by its branch.
    Worktree { path: PathBuf, label: String },
    /// A worktree Projects does not list, whose changes this window cannot
    /// read.
    Unlisted { label: String },
    /// A pull request with no worktree here, which has no local changes or
    /// files to show.
    Elsewhere {
        label: String,
        repository: GitHubRepoKey,
        number: u64,
        url: Option<Url>,
    },
}

impl WorkScope {
    /// What the section beneath the Work list names as its subject, and the
    /// glyph beside the name.
    pub(super) fn header(&self) -> Option<(widget::icon::Handle, &str)> {
        match self {
            Self::Focus => None,
            Self::Worktree { label, .. } | Self::Unlisted { label } => {
                Some((crate::sidebar::worktree_icon(), label))
            }
            Self::Elsewhere { label, .. } => Some((Icon::PullRequest.handle(), label)),
        }
    }
}

/// Above and below the Work and Mentioned sections' lists.
pub(super) const WORK_LIST_PADDING: f32 = space::XS;

/// A pane's conversation's work, as its sections need it.
#[derive(Debug, Default)]
pub(super) struct WorkSummary {
    /// The pieces of work the Work section lists, for its header.
    pub count: usize,
    /// How tall the Work list draws, which bounds its section's height.
    pub height: conversation::Height,
    /// The pull requests the Mentioned section lists, for its header.
    pub mentioned: usize,
    /// How tall the Mentioned list draws.
    pub mentioned_height: conversation::Height,
    shown: Option<WorkKey>,
    pub scope: WorkScope,
}

impl Default for WorkScope {
    fn default() -> Self {
        Self::Focus
    }
}

#[derive(Debug, Default)]
pub struct State {
    /// The conversation of each chat pane whose inspector has been shown,
    /// with the work chosen in it, while the pane stays in the workspace.
    conversations: HashMap<CardId, conversation::State>,
    /// The work chosen in each conversation, whichever pane chose it.
    choices: conversation::Choices,
}

impl State {
    fn conversation(&self, card: CardId) -> Option<&conversation::State> {
        self.conversations.get(&card)
    }

    /// The latest list of pull requests an inspector holds for
    /// `conversation`, with its revision, which its chats label their links
    /// from.
    pub(super) fn conversation_list(
        &self,
        conversation: TerminalId,
    ) -> Option<(u64, &Arc<crate::work::ConversationPullRequests>)> {
        self.conversations
            .values()
            .filter_map(conversation::State::latest_list)
            .filter(|(listed, _, _)| *listed == conversation)
            .map(|(_, revision, list)| (revision, list))
            .max_by_key(|(revision, _)| *revision)
    }
}

impl Cagents {
    pub(super) fn handle_pull_request_ui(&mut self, action: Action) -> Task<Message> {
        if let Action::Conversation(watch) = &action {
            // The conversation's chat labels its links from the same list
            // rather than watching it a second time.
            self.label_chat_pull_request_links(watch);
        }
        match action {
            Action::Conversation(watch) => {
                for state in self.pull_request_inspector.conversations.values_mut() {
                    state.update(watch.clone());
                }
                Task::none()
            }
            Action::Choose { card, key } => {
                let shown_open = self
                    .context_sections
                    .is_open(context_sections::Section::Workspace);
                let inspector = &mut self.pull_request_inspector;
                let Some(state) = inspector.conversations.get(&card) else {
                    return Task::none();
                };
                state.choose(&mut inspector.choices, key, shown_open);
                // Choosing work is asking to see it, here: a pane not focused
                // is focused, so its inspector draws the changes and files.
                let focus = if self.workspace.focused() == Some(card) {
                    Task::none()
                } else {
                    self.focus_workspace_card(card)
                };
                if shown_open {
                    return focus;
                }
                self.context_sections
                    .set_open(context_sections::Section::Workspace, true);
                Task::batch([focus, self.save_settings_task(false)])
            }
            Action::OpenCheck(url) | Action::OpenInBrowser(url) => self.open_clicked_link(url),
            Action::Open {
                repository,
                number,
                url,
            } => {
                let project = self
                    .sidebar_model
                    .projects
                    .iter()
                    .find(|project| {
                        self.github_availability
                            .get(&project.id)
                            .and_then(GitHubAvailability::repository_key)
                            .is_some_and(|own| own.same_repository(&repository))
                    })
                    .map(|project| project.id);
                match (project, self.settings.pull_request_open_target) {
                    (Some(project), PullRequestOpenTarget::Review)
                        if self.pull_request_target(project, number).is_some() =>
                    {
                        self.review_pull_request(project, number)
                    }
                    // A pull request no open project's GitHub state lists can
                    // still be read on GitHub.
                    _ => self.open_clicked_link(url),
                }
            }
        }
    }

    /// The conversation a chat pane holds.
    fn pane_conversation(&self, card: CardId) -> Option<TerminalId> {
        match &self.card_state(card)?.kind {
            CardKind::Chat(session) => Some(session.id),
            _ => None,
        }
    }

    /// The panes on screen whose inspectors are drawn. An inspector the
    /// strip has scrolled away keeps its state but watches nothing, so many
    /// panes cost only what is in view.
    fn mounted_inspectors(&self) -> impl Iterator<Item = CardId> + '_ {
        self.pane_inspectors.visible().filter(|card| {
            self.workspace
                .visibility(*card)
                .is_some_and(crate::workspace::VisibilityClass::is_mounted)
        })
    }

    /// The mounted inspectors' conversations' pull requests and edited
    /// worktrees, pushed by the daemon. A closed inspector watches nothing,
    /// and two panes showing one conversation share its watch.
    pub(super) fn conversation_pull_request_subscription(&self) -> Subscription<Message> {
        let mut conversations: Vec<TerminalId> = self
            .mounted_inspectors()
            .filter_map(|card| self.pane_conversation(card))
            .collect();
        conversations.sort_unstable();
        conversations.dedup();
        Subscription::batch(conversations.into_iter().map(conversation::subscription))
            .map(|watch| Message::PullRequestUi(Action::Conversation(watch)))
    }

    /// Whether a mounted inspector watches `conversation`'s pull requests.
    pub(super) fn inspector_watches(&self, conversation: TerminalId) -> bool {
        self.mounted_inspectors()
            .any(|card| self.pane_conversation(card) == Some(conversation))
    }

    /// Follows each mounted inspector's conversation, and forgets the panes
    /// that have left the workspace.
    pub(super) fn sync_pull_request_inspector(&mut self) {
        let shown: Vec<(CardId, TerminalId)> = self
            .mounted_inspectors()
            .filter_map(|card| Some((card, self.pane_conversation(card)?)))
            .collect();
        let conversations = &mut self.pull_request_inspector.conversations;
        for (card, conversation) in shown {
            conversations
                .entry(card)
                .or_default()
                .retarget(Some(conversation));
        }
        if conversations
            .keys()
            .any(|card| self.workspace.card(*card).is_none())
        {
            conversations.retain(|card, _| self.workspace.card(*card).is_some());
        }
    }

    /// The worktree `card`'s conversation runs in, as Projects knows it.
    fn inspected_here(&self, card: CardId) -> Option<PathBuf> {
        let CardKind::Chat(session) = &self.card_state(card)?.kind else {
            return None;
        };
        let directory = &session.launch.recipe.working_directory;
        self.lists_worktree(directory).then(|| directory.clone())
    }

    /// Whether Projects lists a worktree at `path`, so its changes can be
    /// shown. This runs for each piece of work on every frame, so it reads
    /// the path index the update keeps rather than scanning every row.
    fn lists_worktree(&self, path: &std::path::Path) -> bool {
        self.worktree_paths.lists(path)
    }

    /// Whether `card`'s Work section has anything to list.
    pub(super) fn work_listed(&self, card: CardId) -> bool {
        self.pull_request_inspector
            .conversation(card)
            .is_some_and(conversation::State::lists_work)
    }

    /// Whether `card`'s Mentioned section has anything to list.
    pub(super) fn mentions_listed(&self, card: CardId) -> bool {
        self.settings.show_mentioned_pull_requests
            && self
                .pull_request_inspector
                .conversation(card)
                .is_some_and(conversation::State::mentions_any)
    }

    /// What `card`'s Work and Mentioned sections and the section beneath
    /// them need of its conversation's work, worked out once per use.
    pub(super) fn work_summary(&self, card: CardId) -> WorkSummary {
        let Some(conversation) = self.pull_request_inspector.conversation(card) else {
            return WorkSummary::default();
        };
        let mentioned = conversation.mentioned(self.settings.show_mentioned_pull_requests);
        let listed = conversation.lists_work();
        if !listed && mentioned.is_empty() {
            return WorkSummary::default();
        }
        let here = self.inspected_here(card);
        let openable = |path: &std::path::Path| self.lists_worktree(path);
        let work = conversation.work(here.as_deref(), &openable);
        let choices = &self.pull_request_inspector.choices;
        let shown = conversation.shown(choices, &work, &mentioned, here.as_deref(), &openable);
        // The list's notes go under its work, or under its mentions when it
        // has no work to list.
        let height = conversation::height(conversation, choices, &work, shown, listed);
        let mentioned_height =
            conversation::height(conversation, choices, &mentioned, shown, !listed);
        let scope = match shown {
            None => WorkScope::Focus,
            Some(shown) => match (shown.worktree(), shown.pull_request()) {
                (Some(path), _) if openable(path) => WorkScope::Worktree {
                    path: path.to_path_buf(),
                    label: shown.scope_label(),
                },
                (Some(_), _) => WorkScope::Unlisted {
                    label: shown.scope_label(),
                },
                (None, Some(row)) => WorkScope::Elsewhere {
                    label: shown.scope_label(),
                    repository: row.repository.clone(),
                    number: row.number,
                    url: Url::parse(&row.url()).ok(),
                },
                (None, None) => WorkScope::Focus,
            },
        };
        WorkSummary {
            count: work
                .iter()
                .filter(|work| !matches!(work, conversation::Work::Here(_)))
                .count(),
            height,
            mentioned: mentioned.len(),
            mentioned_height,
            shown: shown.map(|work| work.key()),
            scope,
        }
    }

    /// The worktree the source-control and file panels are pinned to, in
    /// place of the focused card's: the work shown in the inspector that
    /// draws them, or, while that inspector is another pane's than the
    /// focused one, that pane's own worktree. Only a worktree Projects lists
    /// is pinned, so the panels never show one thing under another's name.
    pub(super) fn inspector_panel_pin(&self) -> Option<PathBuf> {
        let owner = self.pane_inspectors.source_control()?;
        match self.work_summary(owner).scope {
            WorkScope::Worktree { path, .. } => Some(path),
            WorkScope::Focus if self.workspace.focused() != Some(owner) => self
                .card_state(owner)
                .and_then(super::worktree_ui::card_directory)
                .map(Path::to_path_buf),
            WorkScope::Focus | WorkScope::Unlisted { .. } | WorkScope::Elsewhere { .. } => None,
        }
    }

    /// `card`'s Work section's list.
    pub(super) fn work_section(&self, card: CardId, summary: &WorkSummary) -> Element<'_, Message> {
        self.work_list(card, summary, false)
    }

    /// `card`'s Mentioned section's list.
    pub(super) fn mentioned_section(
        &self,
        card: CardId,
        summary: &WorkSummary,
    ) -> Element<'_, Message> {
        self.work_list(card, summary, true)
    }

    fn work_list(
        &self,
        card: CardId,
        summary: &WorkSummary,
        mentions: bool,
    ) -> Element<'_, Message> {
        let Some(conversation) = self.pull_request_inspector.conversation(card) else {
            return iced::widget::Space::new().into();
        };
        let here = self.inspected_here(card);
        let openable = |path: &std::path::Path| self.lists_worktree(path);
        let listed = conversation.lists_work();
        let rows = if mentions {
            conversation.mentioned(self.settings.show_mentioned_pull_requests)
        } else {
            conversation.work(here.as_deref(), &openable)
        };
        let list = conversation::view(
            card,
            conversation,
            &self.pull_request_inspector.choices,
            &rows,
            summary.shown.as_ref(),
            self.context_sections
                .is_open(context_sections::Section::Workspace),
            listed != mentions,
            &openable,
        );
        let content: Element<'_, Action> = widget::scrollable(
            widget::container(list)
                .padding(design::list::scroll_padding(WORK_LIST_PADDING))
                .width(Length::Fill),
        )
        .direction(iced::widget::scrollable::Direction::Vertical(
            design::scrollbar(),
        ))
        .width(Length::Fill)
        .into();
        content.map(Message::PullRequestUi)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::work::Relation;

    /// A chat pane on `conversation`, running in /tmp.
    fn chat_card(conversation: TerminalId) -> CardState {
        CardState {
            id: CardId::new(),
            generation: 0,
            title: "Fix the parser".into(),
            width_fraction: 0.5,
            existence: Existence::Open,
            kind: CardKind::Chat(crate::chat::Session {
                native_continuity: Default::default(),
                owned_codex_listener: None,
                id: conversation,
                launch: crate::chat::Launch {
                    provider: crate::agent::Provider::Claude,
                    recipe: crate::model::LaunchRecipe {
                        unset_environment: Default::default(),
                        program: "/bin/sh".into(),
                        arguments: Vec::new(),
                        working_directory: PathBuf::from("/tmp"),
                        environment: Default::default(),
                    },
                    session_id: None,
                },
                proof: None,
                terminal: None,
            }),
        }
    }

    /// The panes of [`conversation_app`].
    pub(super) struct Panes {
        pub conversation: TerminalId,
        pub chat: CardId,
        pub terminal: CardId,
    }

    /// A window with a chat pane, focused, its inspector open, and a
    /// terminal pane beside it, both on screen.
    pub(super) fn conversation_app() -> (Cagents, Panes) {
        let mut app = super::super::tests::context_inspector_test_app(false);
        let conversation = TerminalId::new();
        let chat = chat_card(conversation);
        let terminal = super::super::tests::terminal_card_state();
        let terminal_id = terminal.id;
        let chat_id = chat.id;
        app.durable = Arc::new(WorkspaceState {
            cards: vec![chat, terminal].into(),
            focused: Some(chat_id),
            ..WorkspaceState::default()
        });
        app.workspace = Cagents::rebuild_workspace(&app.durable);
        drop(app.workspace.resize_viewport(1600.0));
        app.index_cards();
        app.context_sections = context_sections::Sections::default();
        app.open_pane_inspector_now(chat_id);
        (
            app,
            Panes {
                conversation,
                chat: chat_id,
                terminal: terminal_id,
            },
        )
    }

    fn listed(
        conversation: TerminalId,
        revision: u64,
        rows: &[(u64, Relation)],
        first: bool,
    ) -> Action {
        listed_with(conversation, revision, rows, Vec::new(), first)
    }

    pub(super) fn listed_with(
        conversation: TerminalId,
        revision: u64,
        rows: &[(u64, Relation)],
        worktrees: Vec<crate::work::ConversationWorktree>,
        first: bool,
    ) -> Action {
        let repository = GitHubRepoKey::new("listed-inspector.invalid", None, "o", "r").unwrap();
        Action::Conversation(conversation::Watch::Listed {
            conversation,
            revision,
            list: Arc::new(crate::work::ConversationPullRequests {
                generated_at_unix_ms: 0,
                repository: Some(repository.clone()),
                pull_requests: rows
                    .iter()
                    .map(|(number, relation)| crate::work::ConversationPullRequest {
                        repository: repository.clone(),
                        number: *number,
                        relation: *relation,
                        by_subagent: false,
                        current_branch: false,
                        pull_request: None,
                        observed_at_unix_ms: None,
                        diff: None,
                        checks: Vec::new(),
                    })
                    .collect(),
                transcript: crate::work::TranscriptCoverage::Complete,
                forge_error: None,
                worktrees,
            }),
            first,
        })
    }

    /// The work chosen in a conversation belongs to the conversation, not
    /// to the pane that chose it: closing the pane and showing the
    /// conversation in another keeps it, as the window's one inspector did.
    #[test]
    fn work_chosen_in_a_pane_outlives_the_pane() {
        let (
            mut app,
            Panes {
                conversation, chat, ..
            },
        ) = conversation_app();
        app.sync_pull_request_inspector();
        let rows = [(4, Relation::Created), (5, Relation::Pushed)];
        drop(app.handle_pull_request_ui(listed(conversation, 1, &rows, true)));
        let key = WorkKey::PullRequest {
            repository: GitHubRepoKey::new("listed-inspector.invalid", None, "o", "r").unwrap(),
            number: 5,
        };
        assert_ne!(app.work_summary(chat).shown.as_ref(), Some(&key));
        drop(app.handle_pull_request_ui(Action::Choose {
            card: chat,
            key: key.clone(),
        }));
        assert_eq!(app.work_summary(chat).shown.as_ref(), Some(&key));

        let reopened = chat_card(conversation);
        let reopened_id = reopened.id;
        let durable = Arc::make_mut(&mut app.durable);
        durable.cards.retain(|card| card.id != chat);
        durable.cards.push(reopened);
        app.workspace = Cagents::rebuild_workspace(&app.durable);
        drop(app.workspace.resize_viewport(1600.0));
        app.index_cards();
        app.sync_pane_inspectors();
        app.open_pane_inspector_now(reopened_id);
        app.sync_pull_request_inspector();
        assert!(app.pull_request_inspector.conversation(chat).is_none());
        drop(app.handle_pull_request_ui(listed(conversation, 2, &rows, true)));
        assert_eq!(app.work_summary(reopened_id).shown.as_ref(), Some(&key));
    }

    #[test]
    fn work_lists_what_the_conversation_did_and_mentions_have_their_own_section() {
        let (
            mut app,
            Panes {
                conversation,
                chat,
                terminal,
            },
        ) = conversation_app();
        app.sync_pull_request_inspector();
        assert!(!app.work_listed(chat));
        drop(app.handle_pull_request_ui(listed(
            conversation,
            1,
            &[(4, Relation::Created), (5, Relation::Mentioned)],
            true,
        )));
        // Mentions are left out unless the settings ask for them, and then
        // are listed in their own section rather than counted with the work.
        assert!(!app.settings.show_mentioned_pull_requests);
        assert!(app.work_listed(chat));
        assert!(!app.mentions_listed(chat));
        let unexpanded = |height: conversation::Height| height.content - height.expanded;
        let row = design::list::TWO_LINE_HEIGHT;
        let summary = app.work_summary(chat);
        assert_eq!((summary.count, summary.mentioned), (1, 0));
        assert_eq!(unexpanded(summary.height), row);
        app.settings.show_mentioned_pull_requests = true;
        assert!(app.mentions_listed(chat));
        let summary = app.work_summary(chat);
        assert_eq!((summary.count, summary.mentioned), (1, 1));
        assert_eq!(
            unexpanded(summary.height),
            row,
            "the work list is unchanged"
        );
        assert_eq!(unexpanded(summary.mentioned_height), row);

        // A conversation that only mentioned pull requests lists no work,
        // but its mentions.
        drop(app.handle_pull_request_ui(listed(
            conversation,
            2,
            &[(5, Relation::Mentioned)],
            false,
        )));
        assert!(!app.work_listed(chat));
        assert!(app.mentions_listed(chat));
        app.settings.show_mentioned_pull_requests = false;
        assert!(!app.mentions_listed(chat));

        // Focusing another pane keeps this pane's inspector on its
        // conversation, and the other pane's lists nothing of it.
        drop(app.workspace.focus(terminal));
        app.sync_pane_inspectors();
        app.sync_pull_request_inspector();
        drop(app.handle_pull_request_ui(listed(conversation, 3, &[(6, Relation::Pushed)], false)));
        assert_eq!(app.work_summary(chat).count, 1);
        assert!(!app.work_listed(terminal));
    }

    #[test]
    fn each_pane_inspector_lists_its_own_conversation() {
        let (
            mut app,
            Panes {
                conversation, chat, ..
            },
        ) = conversation_app();
        let other_conversation = TerminalId::new();
        let other = chat_card(other_conversation);
        let other_id = other.id;
        // Beside the chat, so both panes are on screen.
        Arc::make_mut(&mut app.durable).cards.insert(1, other);
        app.workspace = Cagents::rebuild_workspace(&app.durable);
        drop(app.workspace.resize_viewport(2400.0));
        app.index_cards();
        app.open_pane_inspector_now(other_id);
        app.sync_pull_request_inspector();
        assert!(app.inspector_watches(conversation) && app.inspector_watches(other_conversation));
        drop(app.handle_pull_request_ui(listed(conversation, 1, &[(4, Relation::Created)], true)));
        drop(app.handle_pull_request_ui(listed(
            other_conversation,
            1,
            &[(8, Relation::Pushed), (9, Relation::Created)],
            true,
        )));
        assert_eq!(app.work_summary(chat).count, 1);
        assert_eq!(app.work_summary(other_id).count, 2);

        // Closing one pane's inspector stops only its watch.
        drop(app.set_pane_inspector_open(other_id, false));
        app.settle_pane_inspectors();
        app.sync_pull_request_inspector();
        assert!(app.inspector_watches(conversation));
        assert!(!app.inspector_watches(other_conversation));

        // A pane gone from the workspace is forgotten.
        Arc::make_mut(&mut app.durable)
            .cards
            .retain(|card| card.id != other_id);
        app.workspace = Cagents::rebuild_workspace(&app.durable);
        app.index_cards();
        app.sync_pull_request_inspector();
        assert!(app.pull_request_inspector.conversation(other_id).is_none());
        assert!(app.pull_request_inspector.conversation(chat).is_some());
    }

    #[test]
    fn a_collapsed_workspace_section_reads_nothing_once_the_conversation_leads() {
        let (
            mut app,
            Panes {
                conversation, chat, ..
            },
        ) = conversation_app();
        app.sync_pull_request_inspector();
        app.context_sections
            .set_open(context_sections::Section::Workspace, false);
        drop(app.handle_pull_request_ui(listed_with(
            conversation,
            1,
            &[],
            vec![edited("/tmp/edited", None)],
            true,
        )));
        assert_eq!(app.work_summary(chat).count, 1);
        assert_eq!(app.workspace_view_shown(), None);
        let _ = app.update(Message::ContextSection(context_sections::Action::Toggle(
            context_sections::Section::Workspace,
        )));
        assert!(app.workspace_view_shown().is_some());
    }

    #[test]
    fn an_inspector_left_drawing_source_control_pins_it_to_its_own_pane() {
        let (mut app, Panes { chat, terminal, .. }) = conversation_app();
        if let Some(card) = Arc::make_mut(&mut app.durable)
            .cards
            .iter_mut()
            .find(|card| card.id == terminal)
            && let CardKind::Terminal(session) = &mut card.kind
        {
            session.launch.working_directory = PathBuf::from("/elsewhere");
        }
        app.index_cards();
        app.sync_pull_request_inspector();
        assert!(app.pane_shows_source_control(chat));
        // Its own pane focused, the panels follow focus as they always did.
        assert_eq!(app.inspector_panel_pin(), None);
        // Focus moves to a pane whose inspector is closed: the open one still
        // draws source control, so it stays bound to that pane's worktree
        // rather than to the newly focused one.
        drop(app.workspace.focus(terminal));
        app.sync_pane_inspectors();
        assert!(app.pane_shows_source_control(chat));
        assert_eq!(app.inspector_panel_pin(), Some(PathBuf::from("/tmp")));
        // Opening the focused pane's inspector moves source control there.
        app.open_pane_inspector_now(terminal);
        assert!(app.pane_shows_source_control(terminal));
        assert_eq!(app.inspector_panel_pin(), None);
    }

    fn edited(path: &str, pull_request: Option<u64>) -> crate::work::ConversationWorktree {
        crate::work::ConversationWorktree {
            path: PathBuf::from(path),
            branch: Some(path.rsplit('/').next().unwrap().into()),
            by_subagent: false,
            changed_paths: Some(2),
            diff: Some(crate::work::DiffStat {
                additions: 3,
                deletions: 1,
            }),
            pull_request,
        }
    }

    #[test]
    fn choosing_work_scopes_the_changes_to_it_and_work_not_here_says_so() {
        let (
            mut app,
            Panes {
                conversation, chat, ..
            },
        ) = conversation_app();
        // Projects lists /repo/main and /repo/fix-sidebar.
        app.sidebar_model = crate::sidebar::tests::panel_model();
        // As the update that loads Projects does.
        app.refresh_sidebar_worktree_panes();
        app.sync_pull_request_inspector();
        app.context_sections
            .set_open(context_sections::Section::Workspace, false);
        drop(app.handle_pull_request_ui(listed_with(
            conversation,
            1,
            &[(4, Relation::Created), (7, Relation::Pushed)],
            vec![
                edited("/elsewhere/on-four", Some(4)),
                edited("/repo/fix-sidebar", None),
                edited("/elsewhere/alone", None),
            ],
            true,
        )));
        // Two pull requests and the worktrees no pull request heads.
        assert_eq!(app.work_summary(chat).count, 4);
        // Nothing chosen and the chat runs in none of them: the first one
        // Projects lists is shown, not #4's worktree it does not.
        assert_eq!(
            app.work_summary(chat).scope,
            WorkScope::Worktree {
                path: PathBuf::from("/repo/fix-sidebar"),
                label: "fix-sidebar".into()
            }
        );
        assert_eq!(
            app.inspector_panel_pin(),
            Some(PathBuf::from("/repo/fix-sidebar"))
        );

        // Chosen, a worktree Projects does not list is named for what it
        // is, and the panels pin nothing rather than show another's changes
        // under its name.
        drop(app.handle_pull_request_ui(Action::Choose {
            card: chat,
            key: WorkKey::Worktree(PathBuf::from("/elsewhere/alone")),
        }));
        assert!(
            app.context_sections
                .is_open(context_sections::Section::Workspace),
            "choosing work opens what it shows"
        );
        assert_eq!(
            app.work_summary(chat).scope,
            WorkScope::Unlisted {
                label: "alone".into()
            }
        );
        assert_eq!(app.inspector_panel_pin(), None);

        let repository = GitHubRepoKey::new("listed-inspector.invalid", None, "o", "r").unwrap();
        drop(app.handle_pull_request_ui(Action::Choose {
            card: chat,
            key: WorkKey::PullRequest {
                repository,
                number: 7,
            },
        }));
        assert!(matches!(
            app.work_summary(chat).scope,
            WorkScope::Elsewhere { number: 7, .. }
        ));
        assert_eq!(app.inspector_panel_pin(), None);

        // A hidden inspector pins nothing, so the rail follows focus again.
        drop(app.handle_pull_request_ui(Action::Choose {
            card: chat,
            key: WorkKey::Worktree(PathBuf::from("/repo/fix-sidebar")),
        }));
        assert!(app.inspector_panel_pin().is_some());
        drop(app.rebind_source_control());
        let pinned = Some(PathBuf::from("/repo/fix-sidebar"));
        let active = |app: &Cagents| {
            app.sidebar_state
                .active_worktree()
                .map(|key| key.canonical_path.clone())
        };
        assert_eq!(active(&app), pinned);
        // What the panels bind with nothing pinned: the focused card's
        // worktree.
        let unpinned = {
            let mut state = app.sidebar_state.clone();
            state.pin_panel_worktree(None);
            state.sync_active_worktree(&app.sidebar_model, &app.sidebar_focus());
            state
                .active_worktree()
                .map(|key| key.canonical_path.clone())
        };
        assert_ne!(unpinned, pinned, "the test needs focus elsewhere");
        drop(app.set_pane_inspector_open(chat, false));
        // The close finishes on an animation frame, which rebinds nothing
        // else; the panels must still let go of the work.
        drop(app.update(Message::Frame(
            std::time::Instant::now() + super::super::DIALOG_TRANSITION_DURATION * 2,
        )));
        assert!(!app.pane_inspectors.is_visible(chat));
        assert_eq!(app.inspector_panel_pin(), None);
        assert_eq!(active(&app), unpinned);
    }
}

/// What the Work section costs a frame in a workspace the size of a
/// long-lived installation, with the conversation's work in its last
/// worktrees. The timing run is ignored by default; build it optimized:
///
/// ```sh
/// scripts/cargo-gate test --locked --profile local --no-default-features --features ui \
///     --lib -- --ignored --nocapture app::pull_request_ui::workload
/// ```
#[cfg(test)]
mod workload {
    use super::*;
    use crate::work::{ConversationWorktree, Relation};
    use std::time::{Duration, Instant};

    const WORKTREES: usize = 3000;

    #[test]
    #[ignore = "timing workload; run optimized with --ignored --nocapture"]
    fn work_section_frame_cost() {
        let (
            mut app,
            super::tests::Panes {
                conversation, chat, ..
            },
        ) = super::tests::conversation_app();
        let paths: Vec<PathBuf> = (0..WORKTREES)
            .map(|index| PathBuf::from(format!("/w/app-wt/wt-{index}")))
            .collect();
        app.sidebar_model
            .projects
            .push(crate::sidebar::SidebarProject::new(
                crate::model::ProjectId::new(),
                crate::repository::RepositoryId::new(),
                "app".into(),
                PathBuf::from("/w/app/.git"),
                paths
                    .iter()
                    .map(|path| {
                        crate::sidebar::SidebarWorktree::new(
                            crate::model::WorktreeId::new(),
                            path.clone(),
                            Some("feature".into()),
                            false,
                            false,
                            None,
                            None,
                            None,
                        )
                    })
                    .collect(),
                vec![],
            ));
        // The chat runs in the last worktree; its work is in the ones before.
        if let Some(card) = Arc::make_mut(&mut app.durable)
            .cards
            .iter_mut()
            .find(|card| card.id == chat)
            && let CardKind::Chat(session) = &mut card.kind
        {
            session.launch.recipe.working_directory = paths[WORKTREES - 1].clone();
        }
        app.workspace = Cagents::rebuild_workspace(&app.durable);
        drop(app.workspace.resize_viewport(1600.0));
        app.index_cards();
        app.refresh_sidebar_worktree_panes();
        app.sync_pull_request_inspector();
        let worktree = |index: usize, pull_request| ConversationWorktree {
            path: paths[index].clone(),
            branch: Some(format!("feature/{index}")),
            by_subagent: false,
            changed_paths: Some(3),
            diff: None,
            pull_request,
        };
        let rows: Vec<(u64, Relation)> = (1..=4)
            .map(|number| (number, Relation::Pushed))
            .chain((10..30).map(|number| (number, Relation::Mentioned)))
            .collect();
        let worktrees = (1..=4)
            .map(|number| worktree(WORKTREES - 1 - number as usize, Some(number)))
            .chain([worktree(WORKTREES - 8, None), worktree(WORKTREES - 9, None)])
            .collect();
        drop(app.handle_pull_request_ui(super::tests::listed_with(
            conversation,
            1,
            &rows,
            worktrees,
            true,
        )));
        app.settings.show_mentioned_pull_requests = true;
        assert_eq!(app.work_summary(chat).count, 6);

        let time = |label: &str, run: &dyn Fn()| {
            for _ in 0..16 {
                run();
            }
            let mut samples: Vec<Duration> = (0..256)
                .map(|_| {
                    let started = Instant::now();
                    run();
                    started.elapsed()
                })
                .collect();
            let summary = crate::performance::LatencySummary::from_durations(&mut samples).unwrap();
            eprintln!(
                "{label:<24} median {:>9.2?}  p95 {:>9.2?}  worst {:>9.2?}",
                summary.median, summary.p95, summary.worst
            );
        };
        time("work summary", &|| {
            drop(std::hint::black_box(app.work_summary(chat)))
        });
        time("panel pin (per update)", &|| {
            drop(std::hint::black_box(app.inspector_panel_pin()))
        });
        time("sections view", &|| {
            drop(std::hint::black_box(app.context_sections_view(
                chat,
                360.0,
                app.pane_inspector_toggle(chat, true),
            )))
        });
    }
}
