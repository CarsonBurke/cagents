//! The inspector's Pull requests section: the pull requests the inspected
//! conversation worked on, and, when the settings ask, those it only
//! mentioned. The daemon keeps the list and each pull request's state,
//! changes and checks current; a row opens its pull request where Projects
//! opens one.

use super::*;

mod conversation;

pub(super) use conversation::{Placement, separated, worktree_facts};

#[derive(Debug, Clone)]
pub enum Action {
    /// The focused conversation's pull requests changed.
    Conversation(conversation::Watch),
    /// Open a listed pull request, in the review pane or on GitHub as the
    /// settings choose.
    Open {
        repository: GitHubRepoKey,
        number: u64,
        url: Url,
    },
    /// Open this pull request's checks in a popup, or with `None` close it.
    ShowChecks(Option<(GitHubRepoKey, u64)>),
    /// Open one check's details page.
    OpenCheck(Url),
    /// Open the worktree at this path, where the conversation edited a
    /// listed pull request's branch.
    OpenWorktree(PathBuf),
    /// Show every mentioned pull request, or again only the first few.
    ToggleAllMentions,
}

impl Action {
    pub(super) fn allowed_while_modal(&self) -> bool {
        matches!(self, Self::Conversation(..) | Self::ShowChecks(None))
    }
}

#[derive(Debug, Default)]
pub struct State {
    conversation: conversation::State,
}

impl Cagents {
    pub(super) fn handle_pull_request_ui(&mut self, action: Action) -> Task<Message> {
        let conversation = &mut self.pull_request_inspector.conversation;
        match action {
            Action::Conversation(watch) => {
                conversation.update(watch);
                Task::none()
            }
            Action::ToggleAllMentions => {
                conversation.all_mentions = !conversation.all_mentions;
                Task::none()
            }
            Action::ShowChecks(shown) => {
                conversation.show_checks(shown);
                Task::none()
            }
            Action::OpenCheck(url) => {
                conversation.show_checks(None);
                self.open_clicked_link(url)
            }
            Action::OpenWorktree(path) => match self.open_worktree_message(&path) {
                Some(open) => self.update(open),
                None => Task::none(),
            },
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

    /// The chat card whose conversation the inspector lists pull requests
    /// for: the focused one, else the last one focused while it remains in
    /// the workspace.
    fn inspected_conversation(&self) -> Option<(CardId, TerminalId)> {
        let chat = |card: CardId| match &self.card_state(card)?.kind {
            CardKind::Chat(session) => Some((card, session.id)),
            _ => None,
        };
        self.workspace.focused().and_then(chat).or_else(|| {
            self.pull_request_inspector
                .conversation
                .card
                .filter(|card| self.workspace.card(*card).is_some())
                .and_then(chat)
        })
    }

    /// The inspected conversation's pull requests and edited worktrees,
    /// pushed by the daemon while the inspector is shown, so their headers
    /// count them even while collapsed.
    pub(super) fn conversation_pull_request_subscription(&self) -> Subscription<Message> {
        match self.pull_request_inspector.conversation.conversation {
            Some(conversation) if self.context_inspector_visible() => {
                conversation::subscription(conversation)
                    .map(|watch| Message::PullRequestUi(Action::Conversation(watch)))
            }
            _ => Subscription::none(),
        }
    }

    /// Follows the focused conversation.
    pub(super) fn sync_pull_request_inspector(&mut self) {
        let inspected = self.inspected_conversation();
        self.pull_request_inspector.conversation.retarget(inspected);
    }

    /// Whether a pull request's checks popup is on screen, which takes the
    /// inspector and its Pull requests section being open.
    pub(super) fn checks_popup_open(&self) -> bool {
        self.context_inspector_open
            && self
                .context_sections
                .is_open(context_sections::Section::PullRequests)
            && self.pull_request_inspector.conversation.checks_popup_open()
    }

    /// Forgets the checks popup once what draws it is hidden, so it does
    /// not hold Escape while unseen or return when its section comes back.
    pub(super) fn close_checks_popup(&mut self) {
        self.pull_request_inspector.conversation.show_checks(None);
    }

    /// Whether the Pull requests section has anything to list.
    pub(super) fn pull_requests_listed(&self) -> bool {
        self.pull_request_inspector
            .conversation
            .lists_any(self.settings.show_mentioned_pull_requests)
    }

    /// How many pull requests the section lists, for its header.
    pub(super) fn pull_request_count(&self) -> usize {
        self.pull_request_inspector
            .conversation
            .listed(self.settings.show_mentioned_pull_requests)
            .count()
    }

    /// The worktrees the inspected conversation edited that no pull
    /// request row shows, most recently edited first.
    pub(super) fn edited_worktrees(&self) -> Vec<&crate::work::ConversationWorktree> {
        self.pull_request_inspector
            .conversation
            .worktrees_alone()
            .collect()
    }

    pub(super) fn pull_request_section(&self) -> Element<'_, Message> {
        let list = conversation::view(
            &self.pull_request_inspector.conversation,
            self.settings.show_mentioned_pull_requests,
            &|worktree| self.worktree_placement(&worktree.path),
        )
        .unwrap_or_else(|| widget::Space::new().height(0).into());
        let content: Element<'_, Action> = widget::scrollable(
            widget::container(list)
                .padding(design::with_vertical_rail(Padding::from([
                    space::XS,
                    space::S,
                ])))
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

    /// A window with a chat card, focused, and a terminal card beside it.
    fn conversation_app() -> (Cagents, TerminalId, CardId) {
        let mut app = super::super::tests::context_inspector_test_app(false);
        let conversation = TerminalId::new();
        let chat = CardState {
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
        };
        let terminal = super::super::tests::terminal_card_state();
        let terminal_id = terminal.id;
        let chat_id = chat.id;
        app.durable = Arc::new(WorkspaceState {
            cards: vec![chat, terminal].into(),
            focused: Some(chat_id),
            ..WorkspaceState::default()
        });
        app.workspace = Cagents::rebuild_workspace(&app.durable);
        app.index_cards();
        app.context_inspector_open = true;
        app.context_sections = context_sections::Sections::default();
        (app, conversation, terminal_id)
    }

    fn listed(
        conversation: TerminalId,
        revision: u64,
        rows: &[(u64, Relation)],
        first: bool,
    ) -> Action {
        listed_with(conversation, revision, rows, Vec::new(), first)
    }

    fn listed_with(
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

    #[test]
    fn the_section_follows_the_focused_conversation_and_lists_mentions_only_when_asked() {
        let (mut app, conversation, terminal) = conversation_app();
        app.sync_pull_request_inspector();
        assert!(!app.pull_requests_listed());
        drop(app.handle_pull_request_ui(listed(
            conversation,
            1,
            &[(4, Relation::Created), (5, Relation::Mentioned)],
            true,
        )));
        // Mentions are left out unless the settings ask for them.
        assert!(!app.settings.show_mentioned_pull_requests);
        assert!(app.pull_requests_listed());
        assert_eq!(app.pull_request_count(), 1);
        app.settings.show_mentioned_pull_requests = true;
        assert_eq!(app.pull_request_count(), 2);
        app.settings.show_mentioned_pull_requests = false;

        // A conversation that only mentioned pull requests lists none.
        drop(app.handle_pull_request_ui(listed(
            conversation,
            2,
            &[(5, Relation::Mentioned)],
            false,
        )));
        assert!(!app.pull_requests_listed());

        // Focusing another card in the workspace keeps the conversation.
        drop(app.workspace.focus(terminal));
        app.sync_pull_request_inspector();
        drop(app.handle_pull_request_ui(listed(conversation, 3, &[(6, Relation::Pushed)], false)));
        assert_eq!(app.pull_request_count(), 1);
    }

    #[test]
    fn a_collapsed_workspace_section_reads_nothing_once_the_conversation_leads() {
        let (mut app, conversation, _) = conversation_app();
        app.sync_pull_request_inspector();
        app.context_sections
            .set_open(context_sections::Section::Workspace, false);
        drop(app.handle_pull_request_ui(listed_with(
            conversation,
            1,
            &[],
            vec![crate::work::ConversationWorktree {
                path: PathBuf::from("/tmp/edited"),
                branch: Some("fix".into()),
                by_subagent: false,
                changed_paths: Some(2),
                diff: Some(crate::work::DiffStat {
                    additions: 3,
                    deletions: 1,
                }),
                pull_request: None,
            }],
            true,
        )));
        assert_eq!(app.edited_worktrees().len(), 1);
        assert_eq!(app.workspace_view_shown(), None);
        let _ = app.update(Message::ContextSection(context_sections::Action::Toggle(
            context_sections::Section::Workspace,
        )));
        assert!(app.workspace_view_shown().is_some());
    }

    #[test]
    fn escape_closes_the_checks_popup_before_anything_beneath_it() {
        let (mut app, conversation, _) = conversation_app();
        app.sync_pull_request_inspector();
        drop(app.handle_pull_request_ui(listed(conversation, 1, &[(4, Relation::Created)], true)));
        let repository = GitHubRepoKey::new("listed-inspector.invalid", None, "o", "r").unwrap();
        drop(app.handle_pull_request_ui(Action::ShowChecks(Some((repository, 4)))));
        assert!(app.checks_popup_open());
        let escape = keyboard::Key::Named(keyboard::key::Named::Escape);
        drop(app.update(Message::KeyEvent {
            event: keyboard::Event::KeyPressed {
                key: escape.clone(),
                modified_key: escape,
                physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Escape),
                location: keyboard::Location::Standard,
                modifiers: keyboard::Modifiers::empty(),
                text: None,
                repeat: false,
            },
            captured: false,
        }));
        assert!(!app.checks_popup_open());
        assert!(app.context_inspector_open);
    }

    #[test]
    fn a_hidden_checks_popup_is_forgotten_rather_than_kept_behind_its_section() {
        let (mut app, conversation, _) = conversation_app();
        app.sync_pull_request_inspector();
        drop(app.handle_pull_request_ui(listed(conversation, 1, &[(4, Relation::Created)], true)));
        let repository = GitHubRepoKey::new("listed-inspector.invalid", None, "o", "r").unwrap();
        let show = Action::ShowChecks(Some((repository, 4)));

        drop(app.handle_pull_request_ui(show.clone()));
        drop(
            app.update(Message::ContextSection(context_sections::Action::Toggle(
                context_sections::Section::PullRequests,
            ))),
        );
        assert!(!app.checks_popup_open());
        drop(
            app.update(Message::ContextSection(context_sections::Action::Toggle(
                context_sections::Section::PullRequests,
            ))),
        );
        assert!(
            !app.checks_popup_open(),
            "the popup came back with its section"
        );

        drop(app.handle_pull_request_ui(show));
        drop(app.set_context_inspector_open(false));
        drop(app.set_context_inspector_open(true));
        assert!(
            !app.checks_popup_open(),
            "the popup came back with the inspector"
        );
    }

    #[test]
    fn a_worktree_shows_once_on_the_row_of_the_pull_request_its_branch_heads() {
        let (mut app, conversation, _) = conversation_app();
        app.sync_pull_request_inspector();
        let worktree = |path: &str, pull_request| crate::work::ConversationWorktree {
            path: PathBuf::from(path),
            branch: Some(path.trim_start_matches('/').into()),
            by_subagent: false,
            changed_paths: Some(1),
            diff: None,
            pull_request,
        };
        drop(app.handle_pull_request_ui(listed_with(
            conversation,
            1,
            &[(4, Relation::Created), (5, Relation::Mentioned)],
            vec![
                worktree("/on-the-row", Some(4)),
                worktree("/no-pull-request", None),
                // Only a worked-on row takes its worktree.
                worktree("/mentioned", Some(5)),
            ],
            true,
        )));
        assert_eq!(
            app.edited_worktrees()
                .iter()
                .map(|worktree| worktree.path.to_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["/no-pull-request", "/mentioned"]
        );
        // The row's worktree line opens nothing Projects does not list.
        assert!(
            !app.worktree_placement(std::path::Path::new("/on-the-row"))
                .openable
        );
        drop(app.handle_pull_request_ui(Action::OpenWorktree(PathBuf::from("/on-the-row"))));
    }
}
