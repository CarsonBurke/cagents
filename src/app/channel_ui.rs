use super::*;
use crate::agent::ActivityState;
use crate::model::{ChannelId, ProjectChannel};
use crate::sidebar::SidebarChannel;

#[derive(Debug, Clone, Copy)]
pub enum Action {
    Create(ProjectId),
    Open(ChannelId),
}

impl Cagents {
    pub(super) fn new_project_channel(&mut self) -> Task<Message> {
        let project = self
            .durable
            .active_channel
            .and_then(|id| {
                self.durable
                    .channels
                    .iter()
                    .find(|channel| channel.id == id)
            })
            .map(|channel| channel.project_id)
            .or_else(|| {
                self.active_workspace.as_deref().and_then(|path| {
                    self.sidebar_model
                        .worktree_at(path)
                        .map(|key| key.project_id)
                })
            })
            .or_else(|| {
                sidebar::selected_project(&self.sidebar_model, &self.sidebar_state)
                    .map(|project| project.id)
            })
            .or_else(|| {
                self.sidebar_model
                    .projects
                    .first()
                    .map(|project| project.id)
            });
        match project {
            Some(project) => self.handle_channel_ui(Action::Create(project)),
            None => cosmic::task::message(Message::OpenRepository),
        }
    }

    pub(super) fn handle_channel_ui(&mut self, action: Action) -> Task<Message> {
        if self.update_prepared
            || self.modal_is_open()
            || self.focused_repository_is_saving()
            || self.project_ui.deleting()
        {
            return Task::none();
        }
        match action {
            Action::Create(project_id) => {
                if !self
                    .sidebar_model
                    .projects
                    .iter()
                    .any(|project| project.id == project_id)
                {
                    return Task::none();
                }
                let id = ChannelId::new();
                Arc::make_mut(&mut self.durable)
                    .channels
                    .push(ProjectChannel {
                        id,
                        project_id,
                        // Empty means awaiting the conversation's first meaningful title.
                        // Nonempty names, including user edits, are never replaced here.
                        title: String::new(),
                        cards: Vec::new(),
                        focused: None,
                    });
                self.sidebar_state.reveal_project(project_id);
                self.show_workspace_for_action();
                let scope = self.select_channel(id);
                let chat = self.new_native_chat();
                self.persist();
                Task::batch([scope, chat])
            }
            Action::Open(id) => {
                self.show_workspace_for_action();
                self.select_channel(id)
            }
        }
    }

    /// Refresh cached rows after state changes, never during rendering. A channel's
    /// conversation supplies its initial name, but cannot overwrite a saved name.
    pub(super) fn refresh_sidebar_channels(&mut self) {
        let mut renamed = false;
        for index in 0..self.durable.channels.len() {
            let channel = &self.durable.channels[index];
            if !channel.title.is_empty() {
                continue;
            }
            let title = channel.cards.iter().find_map(|id| {
                let card = self.card_state(*id)?;
                if !card.existence.is_open()
                    || !matches!(&card.kind, CardKind::Chat(_))
                    || matches!(card.title.trim(), "" | "Native chat" | "Agent" | "Terminal")
                {
                    return None;
                }
                normalize_card_name(&card.title)
            });
            if let Some(title) = title {
                Arc::make_mut(&mut self.durable).channels[index].title = title;
                renamed = true;
            }
        }
        if renamed {
            self.persist();
        }

        let durable = &self.durable;
        let state = &self.sidebar_state;
        for project in &mut self.sidebar_model.projects {
            let mut index = 0;
            for channel in durable
                .channels
                .iter()
                .filter(|channel| channel.project_id == project.id)
            {
                let title = if channel.title.is_empty() {
                    "New channel"
                } else {
                    &channel.title
                };
                let selected = durable.active_channel == Some(channel.id);
                let mut activity = None;
                let mut unread = false;
                let mut chats = 0;
                for id in &channel.cards {
                    let Some(card) = durable
                        .cards
                        .iter()
                        .find(|card| card.id == *id && card.existence.is_open())
                    else {
                        continue;
                    };
                    if matches!(&card.kind, CardKind::Chat(_)) {
                        chats += 1;
                    }
                    unread |= state.has_card_update(*id);
                    match state.card_activity(*id).map(|activity| activity.state) {
                        Some(ActivityState::Waiting) => activity = Some(ActivityState::Waiting),
                        Some(ActivityState::Working)
                            if activity != Some(ActivityState::Waiting) =>
                        {
                            activity = Some(ActivityState::Working);
                        }
                        _ => {}
                    }
                }
                let unchanged = project.channels.get(index).is_some_and(|row| {
                    row.id == channel.id
                        && row.title == title
                        && row.selected == selected
                        && row.activity == activity
                        && row.unread == unread
                        && row.chats == chats
                });
                if !unchanged {
                    let row = SidebarChannel::new(
                        channel.id,
                        title.to_owned(),
                        selected,
                        activity,
                        unread,
                        chats,
                    );
                    if index < project.channels.len() {
                        project.channels[index] = row;
                    } else {
                        project.channels.push(row);
                    }
                }
                index += 1;
            }
            project.channels.truncate(index);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_waits_for_a_conversation_name_and_preserves_saved_names() {
        let mut app = super::super::tests::context_inspector_test_app(true);
        let card = app.workspace.focused().unwrap();
        let project = ProjectId::new();
        let id = ChannelId::new();
        let durable = Arc::make_mut(&mut app.durable);
        durable.cards[0].title = "Native chat".into();
        // Channel naming reads the conversation card: the fixture's default
        // terminal card never qualifies.
        durable.cards[0].kind = crate::model::CardKind::Chat(chat_session());
        durable.channels.push(ProjectChannel {
            id,
            project_id: project,
            title: String::new(),
            cards: vec![card],
            focused: Some(card),
        });
        app.sidebar_model.projects.push(SidebarProject::new(
            project,
            RepositoryId::new(),
            "project".into(),
            "/repo".into(),
            Vec::new(),
            Vec::new(),
        ));

        app.refresh_sidebar_channels();
        assert_eq!(
            app.sidebar_model.projects[0].channels[0].title,
            "New channel"
        );
        Arc::make_mut(&mut app.durable).cards[0].title = "Explain the parser".into();
        app.refresh_sidebar_channels();
        assert_eq!(
            app.sidebar_model.projects[0].channels[0].title,
            "Explain the parser"
        );

        let durable = Arc::make_mut(&mut app.durable);
        durable.channels[0].title = "My investigation".into();
        durable.cards[0].title = "A later conversation title".into();
        app.refresh_sidebar_channels();
        assert_eq!(
            app.sidebar_model.projects[0].channels[0].title,
            "My investigation"
        );
        assert_eq!(app.durable.channels[0].title, "My investigation");
    }

    fn chat_session() -> crate::chat::Session {
        crate::chat::Session {
            id: crate::model::TerminalId::new(),
            launch: crate::chat::Launch {
                provider: crate::agent::Provider::Omp,
                recipe: crate::model::LaunchRecipe {
                    program: "/bin/sh".into(),
                    arguments: Vec::new(),
                    working_directory: std::path::PathBuf::from("/tmp"),
                    environment: Default::default(),
                },
                session_id: None,
            },
            proof: None,
            terminal: None,
        }
    }

    /// A channel contains chats rather than being one, so its row reports how
    /// many it holds. Only open chat cards count: a terminal that happens to
    /// live in the channel is not a conversation, and a closing card is no
    /// longer there. The channel's own identity is unaffected by either, which
    /// is what keeps a channel-level action distinct from the chats inside it.
    #[test]
    fn a_channel_row_counts_only_the_open_chats_it_contains() {
        let mut app = super::super::tests::context_inspector_test_app(true);
        let project = ProjectId::new();
        let id = ChannelId::new();
        let durable = Arc::make_mut(&mut app.durable);
        let terminal = durable.cards[0].id;
        let first = CardId::new();
        let second = CardId::new();
        let mut chat = durable.cards[0].clone();
        chat.kind = crate::model::CardKind::Chat(chat_session());
        chat.id = first;
        chat.title = "Explain the parser".into();
        durable.cards.push(chat.clone());
        chat.id = second;
        chat.kind = crate::model::CardKind::Chat(chat_session());
        chat.title = "Draft the migration".into();
        durable.cards.push(chat);
        durable.channels.push(ProjectChannel {
            id,
            project_id: project,
            title: "Investigation".into(),
            cards: vec![terminal, first, second],
            focused: Some(first),
        });
        app.sidebar_model.projects.push(SidebarProject::new(
            project,
            RepositoryId::new(),
            "project".into(),
            "/repo".into(),
            Vec::new(),
            Vec::new(),
        ));

        app.refresh_sidebar_channels();
        assert_eq!(app.sidebar_model.projects[0].channels[0].chats, 2);
        assert_eq!(app.sidebar_model.projects[0].channels[0].id, id);

        let durable = Arc::make_mut(&mut app.durable);
        durable
            .cards
            .iter_mut()
            .find(|card| card.id == second)
            .unwrap()
            .existence = Existence::Closing {
            operation_id: crate::model::OperationId::new(),
        };
        app.refresh_sidebar_channels();
        assert_eq!(app.sidebar_model.projects[0].channels[0].chats, 1);
        assert_eq!(app.sidebar_model.projects[0].channels[0].id, id);
    }
}
