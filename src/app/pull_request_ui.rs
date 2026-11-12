//! The inspector's Work section: the pull requests the inspected
//! conversation worked on and the worktrees it edited, and, when the
//! settings ask, the pull requests it only mentioned. The daemon keeps the
//! list and each pull request's state, changes and checks current. Choosing
//! a row binds the inspector's changes and files to that work.

use super::*;

mod conversation;

pub(super) use conversation::WorkKey;

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
    /// Show this work's changes and files in the inspector.
    Choose(WorkKey),
    /// Open one check's details page.
    OpenCheck(Url),
    /// Unfold the mentioned pull requests, or fold them again.
    ToggleMentions,
}

impl Action {
    pub(super) fn allowed_while_modal(&self) -> bool {
        matches!(self, Self::Conversation(..))
    }
}

/// What the inspector's changes and files show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum WorkScope {
    /// The worktree the focused card names, as without a conversation.
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

/// The inspected conversation's work, as its sections need it.
#[derive(Debug, Default)]
pub(super) struct WorkSummary {
    /// The pieces of work listed, for the header.
    pub count: usize,
    /// The rows the list draws, in two-line rows, which bounds its height.
    pub rows: f32,
    /// The part of `rows` the shown work's expansion adds.
    pub expanded: f32,
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
            Action::ToggleMentions => {
                conversation.mentions_open = !conversation.mentions_open;
                Task::none()
            }
            Action::Choose(key) => {
                conversation.choose(key);
                // Choosing work is asking to see it.
                if self
                    .context_sections
                    .is_open(context_sections::Section::Workspace)
                {
                    return Task::none();
                }
                self.context_sections
                    .set_open(context_sections::Section::Workspace, true);
                self.save_settings_task(false)
            }
            Action::OpenCheck(url) => self.open_clicked_link(url),
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

    /// The worktree the inspected conversation runs in, as Projects knows
    /// it.
    fn inspected_here(&self) -> Option<PathBuf> {
        let card = self.pull_request_inspector.conversation.card?;
        let CardKind::Chat(session) = &self.card_state(card)?.kind else {
            return None;
        };
        self.sidebar_model
            .worktree_at(&session.launch.recipe.working_directory)
            .map(|key| key.canonical_path)
    }

    /// Whether the Work section has anything to list.
    pub(super) fn work_listed(&self) -> bool {
        self.pull_request_inspector
            .conversation
            .lists_any(self.settings.show_mentioned_pull_requests)
    }

    /// What the Work section and the section beneath it need of the
    /// inspected conversation's work, worked out once per use.
    pub(super) fn work_summary(&self) -> WorkSummary {
        let conversation = &self.pull_request_inspector.conversation;
        if !self.work_listed() {
            return WorkSummary::default();
        }
        let here = self.inspected_here();
        let openable = |path: &std::path::Path| self.sidebar_model.worktree_at(path).is_some();
        let work = conversation.work(here.as_deref());
        let shown = conversation.shown(&work, here.as_deref(), &openable);
        let mut rows = work.len() as f32 + shown.map_or(0.0, conversation::expanded_rows);
        if self.settings.show_mentioned_pull_requests {
            let mentioned = conversation
                .rows()
                .iter()
                .filter(|row| !row.relation.worked_on())
                .count();
            if mentioned > 0 {
                // The fold, and each one-line row beneath it while open.
                rows += 0.5
                    + if conversation.mentions_open {
                        0.5 * mentioned as f32
                    } else {
                        0.0
                    };
            }
        }
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
            rows,
            expanded: shown.map_or(0.0, conversation::expanded_rows),
            shown: shown.map(|work| work.key()),
            scope,
        }
    }

    /// The worktree the source-control and file panels are pinned to, in
    /// place of the focused card's, while the inspector shows a
    /// conversation's work. Only a worktree Projects lists is pinned, so the
    /// panels never show one thing under another's name.
    pub(super) fn inspector_panel_pin(&self) -> Option<PathBuf> {
        if !self.context_inspector_visible() {
            return None;
        }
        match self.work_summary().scope {
            WorkScope::Worktree { path, .. } => Some(path),
            WorkScope::Focus | WorkScope::Unlisted { .. } | WorkScope::Elsewhere { .. } => None,
        }
    }

    pub(super) fn work_section(&self, summary: &WorkSummary) -> Element<'_, Message> {
        let here = self.inspected_here();
        let conversation = &self.pull_request_inspector.conversation;
        let work = conversation.work(here.as_deref());
        let list = conversation::view(
            conversation,
            &work,
            summary.shown.as_ref(),
            self.context_sections
                .is_open(context_sections::Section::Workspace),
            self.settings.show_mentioned_pull_requests,
            &|path| self.sidebar_model.worktree_at(path).is_some(),
        );
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
        assert!(!app.work_listed());
        drop(app.handle_pull_request_ui(listed(
            conversation,
            1,
            &[(4, Relation::Created), (5, Relation::Mentioned)],
            true,
        )));
        // Mentions are left out unless the settings ask for them, and even
        // then are folded beneath the work rather than counted with it.
        assert!(!app.settings.show_mentioned_pull_requests);
        assert!(app.work_listed());
        let unexpanded = |app: &Cagents| {
            let summary = app.work_summary();
            summary.rows - summary.expanded
        };
        assert_eq!(app.work_summary().count, 1);
        assert_eq!(unexpanded(&app), 1.0);
        app.settings.show_mentioned_pull_requests = true;
        assert_eq!(app.work_summary().count, 1);
        assert_eq!(unexpanded(&app), 1.5, "the folded mentions take half a row");
        drop(app.handle_pull_request_ui(Action::ToggleMentions));
        assert_eq!(unexpanded(&app), 2.0);
        app.settings.show_mentioned_pull_requests = false;

        // A conversation that only mentioned pull requests lists no work.
        drop(app.handle_pull_request_ui(listed(
            conversation,
            2,
            &[(5, Relation::Mentioned)],
            false,
        )));
        assert!(!app.work_listed());

        // Focusing another card in the workspace keeps the conversation.
        drop(app.workspace.focus(terminal));
        app.sync_pull_request_inspector();
        drop(app.handle_pull_request_ui(listed(conversation, 3, &[(6, Relation::Pushed)], false)));
        assert_eq!(app.work_summary().count, 1);
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
            vec![edited("/tmp/edited", None)],
            true,
        )));
        assert_eq!(app.work_summary().count, 1);
        assert_eq!(app.workspace_view_shown(), None);
        let _ = app.update(Message::ContextSection(context_sections::Action::Toggle(
            context_sections::Section::Workspace,
        )));
        assert!(app.workspace_view_shown().is_some());
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
        let (mut app, conversation, _) = conversation_app();
        // Projects lists /repo/main and /repo/fix-sidebar.
        app.sidebar_model = crate::sidebar::tests::panel_model();
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
        assert_eq!(app.work_summary().count, 4);
        // Nothing chosen and the chat runs in none of them: the first one
        // Projects lists is shown, not #4's worktree it does not.
        assert_eq!(
            app.work_summary().scope,
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
        drop(
            app.handle_pull_request_ui(Action::Choose(WorkKey::Worktree(PathBuf::from(
                "/elsewhere/alone",
            )))),
        );
        assert!(
            app.context_sections
                .is_open(context_sections::Section::Workspace),
            "choosing work opens what it shows"
        );
        assert_eq!(
            app.work_summary().scope,
            WorkScope::Unlisted {
                label: "alone".into()
            }
        );
        assert_eq!(app.inspector_panel_pin(), None);

        let repository = GitHubRepoKey::new("listed-inspector.invalid", None, "o", "r").unwrap();
        drop(
            app.handle_pull_request_ui(Action::Choose(WorkKey::PullRequest {
                repository,
                number: 7,
            })),
        );
        assert!(matches!(
            app.work_summary().scope,
            WorkScope::Elsewhere { number: 7, .. }
        ));
        assert_eq!(app.inspector_panel_pin(), None);

        // A hidden inspector pins nothing, so the rail follows focus again.
        drop(
            app.handle_pull_request_ui(Action::Choose(WorkKey::Worktree(PathBuf::from(
                "/repo/fix-sidebar",
            )))),
        );
        assert!(app.inspector_panel_pin().is_some());
        drop(app.set_context_inspector_open(false));
        app.context_inspector_transition = None;
        assert!(!app.context_inspector_visible());
        assert_eq!(app.inspector_panel_pin(), None);
    }
}
