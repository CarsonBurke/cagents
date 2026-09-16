use super::*;
use crate::agent::ActivityState;
use crate::model::{ChannelId, ProjectChannel};
use crate::sidebar::SidebarChannel;
use crate::workspace_model::{ModelEdit, PENDING_ORDINAL};

#[derive(Debug, Clone, Copy)]
pub enum Action {
    Create(ProjectId),
    Open(ChannelId),
    /// Asks to delete a channel. Nothing is removed until the confirmation is
    /// answered, because a channel's conversations exist nowhere else.
    Delete(ChannelId),
    ConfirmDelete,
    Dismiss,
}

impl Action {
    /// Whether this action is the deletion dialog's own answer.
    ///
    /// Everything else a channel can be asked to do is a navigation command
    /// and stands down behind any modal; refusing the dialog's own answers
    /// would strand it with no way out.
    pub fn answers_own_dialog(self) -> bool {
        matches!(self, Self::ConfirmDelete | Self::Dismiss)
    }
}

/// The pending deletion, held while its confirmation is on screen.
#[derive(Debug, Default)]
pub struct State {
    dialog: Option<Deletion>,
}

#[derive(Debug, Clone)]
struct Deletion {
    channel: ChannelId,
    title: String,
    /// Open conversations the deletion closes, reported before it happens.
    chats: usize,
}

impl State {
    pub fn modal_open(&self) -> bool {
        self.dialog.is_some()
    }

    pub(super) fn focus_target() -> widget::Id {
        widget::Id::new("dialog.channel.confirm")
    }
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
            || self.focused_repository_is_saving()
            || self.project_ui.deleting()
            || (self.modal_is_open() && !action.answers_own_dialog())
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
                        // The registry assigns the real rank and this window
                        // learns it from the answer to the publish below. Until
                        // then the channel outranks every ranked one, which is
                        // the truth about something created a moment ago.
                        ordinal: PENDING_ORDINAL,
                        // Not shared until the registry answers the publish
                        // below. Claiming it early would let a registry that
                        // never received the create retire it on a reconnect.
                        shared: false,
                        cards: Vec::new(),
                        focused: None,
                    });
                self.publish_channel_creation(id, project_id);
                self.sidebar_state.reveal_project(project_id);
                self.show_workspace_for_action();
                let scope = self.select_channel(id);
                let chat = self.new_native_chat(self.preferred_chat_provider(), None);
                self.persist();
                Task::batch([scope, chat])
            }
            Action::Open(id) => {
                self.show_workspace_for_action();
                self.select_channel(id)
            }
            Action::Delete(id) => {
                let Some(channel) = self
                    .durable
                    .channels
                    .iter()
                    .find(|channel| channel.id == id)
                else {
                    return Task::none();
                };
                let title = channel_display_title(channel).to_owned();
                let chats = channel
                    .cards
                    .iter()
                    .filter(|card| {
                        self.durable.cards.iter().any(|state| {
                            state.id == **card
                                && state.existence.is_open()
                                && matches!(&state.kind, CardKind::Chat(_))
                        })
                    })
                    .count();
                self.channel_ui.dialog = Some(Deletion {
                    channel: id,
                    title,
                    chats,
                });
                iced::widget::operation::focus(State::focus_target()).map(cosmic::Action::App)
            }
            Action::ConfirmDelete => {
                let Some(deletion) = self.channel_ui.dialog.take() else {
                    return Task::none();
                };
                self.delete_channel(deletion.channel)
            }
            Action::Dismiss => {
                self.channel_ui.dialog = None;
                let focus = self.nested_modal_focus_target().unwrap_or_else(|| {
                    self.workspace
                        .focused()
                        .map(workspace_widget::card_focus_id)
                        .unwrap_or_else(|| widget::Id::new(TOOLBAR_FOCUS_ID))
                });
                iced::widget::operation::focus(focus).map(cosmic::Action::App)
            }
        }
    }

    /// Removes a channel along with everything that existed only inside it.
    ///
    /// A channel's conversations are reachable nowhere else, so they are
    /// retired with it rather than left behind as durable cards no scope can
    /// show - the same treatment worktree deletion gives the cards of the
    /// directory it removes.
    pub(super) fn delete_channel(&mut self, id: ChannelId) -> Task<Message> {
        let Some(index) = self
            .durable
            .channels
            .iter()
            .position(|channel| channel.id == id)
        else {
            return Task::none();
        };
        let project_id = self.durable.channels[index].project_id;
        let ordinal = self.durable.channels[index].ordinal;
        let was_active = self.durable.active_channel == Some(id);
        // The channel below the deleted one in the sidebar, or the one above it
        // when the project's oldest channel is the one going away. Below means
        // the next lower rank, because the sidebar renders newest first.
        let mut below = None::<&ProjectChannel>;
        let mut above = None::<&ProjectChannel>;
        for channel in self
            .durable
            .channels
            .iter()
            .filter(|channel| channel.id != id && channel.project_id == project_id)
        {
            if channel.ordinal <= ordinal {
                if below.is_none_or(|best| channel.ordinal > best.ordinal) {
                    below = Some(channel);
                }
            } else if above.is_none_or(|best| channel.ordinal < best.ordinal) {
                above = Some(channel);
            }
        }
        let successor = below.or(above).map(|channel| channel.id);
        let channel = Arc::make_mut(&mut self.durable).channels.remove(index);
        // Every window loses the channel, not only this one. A deletion that
        // arrived from the registry publishes it right back, which the registry
        // answers as a no-op because the channel is already gone - cheaper than
        // threading an origin flag through the one path that must behave
        // identically whoever asked.
        self.publish_model_edit(ModelEdit::DeleteChannel { id });
        for card in channel.cards {
            self.retire_lsp_card(card);
            self.cards.remove(&card);
            self.lifecycle.remove(card);
            if let Some(state) = self.card_state_mut(card) {
                state.existence = super::project_ui::removed_existence("channel deleted by user");
            }
        }
        let select = if was_active {
            // A scope that no longer exists cannot stay selected, and the
            // workspace has to land on something the user can still reach.
            match successor {
                Some(next) => self.select_channel(next),
                None => match self.project_primary_worktree(project_id) {
                    Some(path) => self.select_worktree(path, false),
                    None => {
                        Arc::make_mut(&mut self.durable).active_channel = None;
                        Task::none()
                    }
                },
            }
        } else {
            Task::none()
        };
        // After the reselection: leaving a scope records its camera against
        // the channel it is leaving, including this one.
        self.channel_offsets.remove(&id);
        self.refresh_sidebar_channels();
        self.persist();
        Task::batch([select, self.lifecycle_sweep(), self.sync_chrome()])
    }

    /// Where a project's workspace falls back to when no channel is selected.
    fn project_primary_worktree(&self, project_id: ProjectId) -> Option<PathBuf> {
        let project = self.sidebar_model.project(project_id)?;
        Some(
            project
                .worktrees
                .iter()
                .find(|worktree| sidebar::is_primary_worktree(&project.repository, &worktree.path))
                .map_or_else(
                    || project.repository.clone(),
                    |worktree| worktree.path.clone(),
                ),
        )
    }

    pub(super) fn channel_dialog_view(&self) -> Option<Element<'_, Message>> {
        let dialog = self.channel_ui.dialog.as_ref()?;
        let rows: Vec<Element<'_, Message>> = vec![
            widget::text::title3("Delete channel?").into(),
            super::notice_body(dialog.title.clone()),
            super::notice_body(match dialog.chats {
                0 => "It holds no conversations.".to_owned(),
                1 => "Deleting it closes the one conversation inside it.".to_owned(),
                chats => format!("Deleting it closes the {chats} conversations inside it."),
            }),
            super::notice_detail(
                "The channel and the conversations it holds are removed from this workspace. \
                 Nothing outside the channel is touched.",
            ),
            widget::row::with_children(vec![
                iced::widget::Space::new().width(Length::Fill).into(),
                widget::button::standard("Cancel")
                    .id(widget::Id::new("dialog.channel.close"))
                    .on_press(Message::ChannelUi(Action::Dismiss))
                    .into(),
                widget::button::destructive("Delete channel")
                    .id(State::focus_target())
                    .on_press(Message::ChannelUi(Action::ConfirmDelete))
                    .into(),
            ])
            .spacing(cosmic::theme::spacing().space_xxs)
            .into(),
        ];
        Some(super::dialog_ui::surface(
            widget::column::with_children(rows).spacing(3.0 * design::GRID_UNIT),
            480.0,
        ))
    }

    /// Refresh cached rows after state changes, never during rendering. A channel's
    /// conversation supplies its initial name, but cannot overwrite a saved name.
    pub(super) fn refresh_sidebar_channels(&mut self) {
        let mut renamed = Vec::new();
        for index in 0..self.durable.channels.len() {
            let channel = &self.durable.channels[index];
            if !channel.title.is_empty() {
                continue;
            }
            let title = channel.cards.iter().find_map(|id| {
                let card = self.card_state(*id)?;
                if !card.existence.is_open()
                    || !matches!(&card.kind, CardKind::Chat(_))
                    || super::chat_ui::placeholder_card_title(&card.title)
                {
                    return None;
                }
                normalize_card_name(&card.title)
            });
            if let Some(title) = title {
                let durable = Arc::make_mut(&mut self.durable);
                durable.channels[index].title = title.clone();
                renamed.push((durable.channels[index].id, title));
            }
        }
        if !renamed.is_empty() {
            // The window that derives a title is the shared title's writer for
            // that moment, and a window that receives one adopts it even where
            // its own derivation would differ. The guard above is what stops a
            // later derivation from fighting a title that already exists.
            for (id, title) in renamed {
                self.publish_model_edit(ModelEdit::RetitleChannel { id, title });
            }
            self.persist();
        }

        let durable = &self.durable;
        let state = &self.sidebar_state;
        for project in &mut self.sidebar_model.projects {
            let mut index = 0;
            // Newest first. The durable vector keeps the registry's ascending
            // creation ranks, because that is the order the registry assigns and
            // the order a seed donates; display inverts it, because a channel is
            // created to be used now and belongs where the pointer already is.
            for channel in durable
                .channels
                .iter()
                .filter(|channel| channel.project_id == project.id)
                .rev()
            {
                let title = channel_display_title(channel);
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

/// A channel still waiting for its conversation's name has to be nameable
/// anyway: a row has to label it, and a dialog about to destroy it has to say
/// which one it means.
fn channel_display_title(channel: &ProjectChannel) -> &str {
    if channel.title.is_empty() {
        "New channel"
    } else {
        &channel.title
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A channel is created to be used now, so the newest belongs at the top of
    /// its project rather than at the bottom of a list that only grows.
    #[test]
    fn a_newly_created_channel_renders_above_the_ones_before_it() {
        let mut app = super::super::tests::context_inspector_test_app(true);
        let project = ProjectId::new();
        app.sidebar_model.projects.push(SidebarProject::new(
            project,
            RepositoryId::new(),
            "project".into(),
            "/repo".into(),
            vec![crate::sidebar::SidebarWorktree::from_durable(
                crate::model::WorktreeId::new(),
                "/repo".into(),
                Some("main".into()),
            )],
            Vec::new(),
        ));

        let _ = app.handle_channel_ui(Action::Create(project));
        let first = app.durable.active_channel.expect("the first channel opens");
        let _ = app.handle_channel_ui(Action::Create(project));
        let second = app
            .durable
            .active_channel
            .expect("the second channel opens");
        assert_ne!(first, second);

        app.refresh_sidebar_channels();
        let rows = &app.sidebar_model.projects[0].channels;
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].id, second, "the newest channel is the top row");
        assert_eq!(rows[1].id, first);
    }

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
            ordinal: 0,
            shared: false,
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
            ordinal: 0,
            shared: false,
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

    /// A channel's conversations are reachable nowhere else, so deleting one
    /// has to take them with it, say so first, and leave the workspace on a
    /// scope that still exists.
    #[test]
    fn deleting_the_active_channel_retires_its_chats_and_lands_on_a_survivor() {
        let mut app = super::super::tests::context_inspector_test_app(true);
        let project = ProjectId::new();
        let doomed = ChannelId::new();
        let survivor = ChannelId::new();
        let conversation = CardId::new();
        let durable = Arc::make_mut(&mut app.durable);
        let mut chat = durable.cards[0].clone();
        chat.id = conversation;
        chat.kind = crate::model::CardKind::Chat(chat_session());
        chat.title = "Explain the parser".into();
        durable.cards.push(chat);
        durable.channels.push(ProjectChannel {
            id: doomed,
            project_id: project,
            title: "Investigation".into(),
            ordinal: 0,
            shared: false,
            cards: vec![conversation],
            focused: Some(conversation),
        });
        durable.channels.push(ProjectChannel {
            id: survivor,
            project_id: project,
            title: "Migration".into(),
            ordinal: 0,
            shared: false,
            cards: Vec::new(),
            focused: None,
        });
        durable.active_channel = Some(doomed);
        app.sidebar_model.projects.push(SidebarProject::new(
            project,
            RepositoryId::new(),
            "project".into(),
            "/repo".into(),
            vec![crate::sidebar::SidebarWorktree::from_durable(
                crate::model::WorktreeId::new(),
                "/repo".into(),
                Some("main".into()),
            )],
            Vec::new(),
        ));
        app.refresh_sidebar_channels();
        assert_eq!(app.sidebar_model.projects[0].channels.len(), 2);

        // Asking is not doing: a channel holding a live conversation says what
        // it is about to close before it closes anything.
        let _ = app.handle_channel_ui(Action::Delete(doomed));
        assert!(app.modal_is_open(), "deletion confirms before it destroys");
        assert!(
            app.dialog().is_some(),
            "and the confirmation reaches the frame the runtime mounts"
        );
        assert!(app.durable.channels.iter().any(|row| row.id == doomed));

        let _ = app.handle_channel_ui(Action::ConfirmDelete);
        assert!(
            !app.durable.channels.iter().any(|row| row.id == doomed),
            "the confirmed deletion removes the channel"
        );
        assert_eq!(
            app.durable.active_channel,
            Some(survivor),
            "a deleted scope cannot stay selected, so a surviving sibling takes over"
        );
        let chat = app
            .durable
            .cards
            .iter()
            .find(|card| card.id == conversation)
            .expect("the card is tombstoned rather than forgotten");
        assert!(
            !chat.existence.is_open(),
            "a conversation that lived only in the channel goes with it"
        );
        assert_eq!(app.sidebar_model.projects[0].channels.len(), 1);
        assert_eq!(
            app.durable.active_worktree, app.active_workspace,
            "the deletion persisted the workspace it left behind"
        );
    }
}
