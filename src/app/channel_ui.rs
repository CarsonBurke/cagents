use super::*;
use crate::agent::ActivityState;
use crate::model::{ChannelId, ProjectChannel};
use crate::sidebar::SidebarChannel;
use crate::workspace_model::ModelEdit;

fn close_channel_sessions(sessions: Vec<CardKind>) -> Result<(), String> {
    if !sessions
        .iter()
        .any(|kind| matches!(kind, CardKind::Terminal(_) | CardKind::Chat(_)))
    {
        return Ok(());
    }
    let socket = crate::terminal::ipc::daemon_socket_path().map_err(|error| error.to_string())?;
    let mut client = match crate::control::ControlClient::connect(&socket) {
        Ok(client) => client,
        Err(crate::control::ControlError::Connect(error))
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) =>
        {
            return Ok(());
        }
        Err(error) => return Err(format!("Cannot close the channel's sessions: {error}")),
    };
    // Query live identities: a saved descriptor can predate an upgrade or handoff.
    let terminals = client.list().map_err(|error| error.to_string())?;
    let chats = client.list_chats().map_err(|error| error.to_string())?;
    let terminal_ids = channel_terminal_ids(&sessions, &chats);
    for kind in &sessions {
        if let CardKind::Chat(session) = kind
            && let Some(live) = chats.iter().find(|live| live.id == session.id)
        {
            if let Err(error) = client.close_chat(live.clone()) {
                // Another window or a natural exit can win this race.
                // Only a fresh inventory may turn a close error into success.
                if client
                    .list_chats()
                    .map_err(|error| error.to_string())?
                    .iter()
                    .any(|chat| chat.id == live.id)
                {
                    return Err(error.to_string());
                }
            }
        }
    }
    for terminal in terminals {
        if !channel_terminal_needs_close(&terminal.session, &terminal_ids) {
            continue;
        }
        let result = client
            .close_terminal_checked(terminal.session.id, true)
            .map_err(|error| error.to_string())
            .and_then(|outcome| match outcome {
                crate::terminal::ipc::TerminalCloseResult::Closed => Ok(()),
                other => Err(format!("Channel terminal could not be closed: {other:?}")),
            });
        if let Err(error) = result {
            let still_running = client
                .list()
                .map_err(|error| error.to_string())?
                .iter()
                .any(|live| {
                    live.session.id == terminal.session.id
                        && channel_terminal_needs_close(&live.session, &terminal_ids)
                });
            if still_running {
                return Err(error);
            }
        }
    }
    Ok(())
}

fn channel_terminal_ids(
    sessions: &[CardKind],
    chats: &[crate::chat::Session],
) -> HashSet<crate::model::TerminalId> {
    sessions
        .iter()
        .filter_map(|kind| match kind {
            CardKind::Terminal(session) => Some(session.id),
            // Codex retains its source PTY when converted to a native chat.
            // Close it after the actor, even if the actor has already exited.
            CardKind::Chat(session) => chats
                .iter()
                .find(|live| live.id == session.id)
                .unwrap_or(session)
                .terminal
                .as_ref()
                .map(|terminal| terminal.id),
            _ => None,
        })
        .collect()
}

fn channel_terminal_needs_close(
    session: &crate::model::TerminalSession,
    ids: &HashSet<crate::model::TerminalId>,
) -> bool {
    ids.contains(&session.id)
        && !matches!(
            session.continuity,
            crate::model::TerminalContinuity::Exited { .. }
                | crate::model::TerminalContinuity::RelaunchAvailable {
                    last_exit: Some(_),
                    ..
                }
        )
}

#[derive(Debug, Clone)]
pub enum Action {
    Create(ProjectId),
    Open(ChannelId),
    /// Deletes an empty channel immediately; live sessions require confirmation.
    Delete(ChannelId),
    ConfirmDelete,
    Dismiss,
    SessionsClosed(ChannelId, Result<(), String>),
    RetryDelete(ChannelId),
}

impl Action {
    /// Whether this action is the deletion dialog's own answer.
    ///
    /// Everything else a channel can be asked to do is a navigation command
    /// and stands down behind any modal; refusing the dialog's own answers
    /// would strand it with no way out.
    pub fn answers_own_dialog(&self) -> bool {
        matches!(
            self,
            Self::ConfirmDelete | Self::Dismiss | Self::SessionsClosed(..) | Self::RetryDelete(_)
        )
    }
}

/// The pending deletion, held while its confirmation is on screen.
#[derive(Debug, Default)]
pub struct State {
    dialog: Option<Deletion>,
    deleting: HashSet<ChannelId>,
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
        self.dialog.is_some() || !self.deleting.is_empty()
    }

    pub(super) fn is_deleting(&self, id: ChannelId) -> bool {
        self.deleting.contains(&id)
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
        if let Action::RetryDelete(id) = action {
            self.channel_ui.deleting.remove(&id);
            return self.delete_channel(id);
        }
        if let Action::SessionsClosed(id, result) = action {
            self.channel_ui.deleting.remove(&id);
            return match result {
                Ok(()) => self.finish_delete_channel(id),
                Err(error) => {
                    self.status = Some(TransientNotice::error(error));
                    Task::none()
                }
            };
        }
        if self.update_prepared
            || self.focused_repository_is_saving()
            || self.project_ui.deleting()
            || (self.modal_is_open() && !action.answers_own_dialog())
        {
            return Task::none();
        }
        match action {
            Action::SessionsClosed(..) | Action::RetryDelete(_) => unreachable!(),
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
                // The registry assigns the real rank and this window learns it
                // from the answer to the publish below. Until then it takes one
                // more than the highest rank it knows, which is what the
                // registry would assign and what keeps two channels created
                // before any answer arrives in the order they were made.
                let ordinal = self.next_provisional_ordinal();
                Arc::make_mut(&mut self.durable)
                    .channels
                    .push(ProjectChannel {
                        id,
                        project_id,
                        // Empty means awaiting the channel's first accepted prompt.
                        // Nonempty names, including user edits, are never replaced here.
                        title: String::new(),
                        ordinal,
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
                let chat = self.new_native_thread();
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
                                && matches!(&state.kind, CardKind::Chat(_) | CardKind::Terminal(_))
                        })
                    })
                    .count();
                if chats == 0 {
                    return self.delete_channel(id);
                }
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

    /// Closes every channel that `retired` has just left without an open card.
    ///
    /// A channel is its conversations; once the last one is closed there is
    /// nothing to come back to, and an empty channel left in the sidebar is
    /// one more row the user has to delete by hand. Only channels one of
    /// these cards belonged to are considered: a channel that has just been
    /// created is empty too, and is waiting for its first card.
    pub(super) fn close_emptied_channels(&mut self, retired: &[CardId]) -> Task<Message> {
        let emptied: Vec<ChannelId> = self
            .durable
            .channels
            .iter()
            .filter(|channel| channel.cards.iter().any(|card| retired.contains(card)))
            .filter(|channel| {
                !channel.cards.iter().any(|&card| {
                    !retired.contains(&card)
                        && self
                            .card_state(card)
                            .is_some_and(|state| state.existence.is_open())
                })
            })
            .map(|channel| channel.id)
            .collect();
        Task::batch(emptied.into_iter().map(|id| self.delete_channel(id)))
    }

    /// Removes a channel along with everything that existed only inside it.
    ///
    /// A channel's conversations are reachable nowhere else, so they are
    /// retired with it rather than left behind as durable cards no scope can
    /// show - the same treatment worktree deletion gives the cards of the
    /// directory it removes.
    pub(super) fn delete_channel(&mut self, id: ChannelId) -> Task<Message> {
        let Some(channel) = self
            .durable
            .channels
            .iter()
            .find(|channel| channel.id == id)
        else {
            return Task::none();
        };
        if !self.channel_ui.deleting.insert(id) {
            return Task::none();
        }
        if channel
            .cards
            .iter()
            .any(|card| self.pending_terminal_spawn(*card) || self.chat_start_pending(*card))
        {
            return cosmic::task::future(async move {
                tokio::time::sleep(Duration::from_millis(50)).await;
                Message::ChannelUi(Action::RetryDelete(id))
            });
        }
        let sessions: Vec<_> = self
            .durable
            .cards
            .iter()
            .filter(|card| channel.cards.contains(&card.id) && card.existence.is_open())
            .map(|card| card.kind.clone())
            .collect();
        if !sessions
            .iter()
            .any(|kind| matches!(kind, CardKind::Terminal(_) | CardKind::Chat(_)))
        {
            self.channel_ui.deleting.remove(&id);
            return self.finish_delete_channel(id);
        }
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || close_channel_sessions(sessions))
                .await
                .map_err(|error| error.to_string())
                .and_then(|result| result);
            Message::ChannelUi(Action::SessionsClosed(id, result))
        })
    }

    fn finish_delete_channel(&mut self, id: ChannelId) -> Task<Message> {
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
            self.sidebar_state.clear_agent_activity(card);
            self.sidebar_state.acknowledge_agent_update(card);
            self.chat_discard(card);
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
        if !self.channel_ui.deleting.is_empty() {
            return Some(super::dialog_ui::surface(
                widget::column::with_children(vec![
                    widget::text::title3("Closing channel…").into(),
                    super::notice_body("Closing its sessions before removing the channel."),
                ])
                .spacing(3.0 * design::GRID_UNIT),
                480.0,
            ));
        }
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
    pub(super) fn channel_needs_name(&self, card: CardId) -> bool {
        self.durable
            .channels
            .iter()
            .any(|channel| channel.title.is_empty() && channel.cards.contains(&card))
    }

    /// Names a new channel from its first submitted prompt or conversation name.
    ///
    /// A title that already exists is authoritative: it may have been entered
    /// by the user, supplied by another window, or restored from persistence.
    pub(super) fn name_channel_from_prompt(&mut self, card: CardId, prompt: &str) {
        let Some(index) = self
            .durable
            .channels
            .iter()
            .position(|channel| channel.title.is_empty() && channel.cards.contains(&card))
        else {
            return;
        };
        let Some(title) = channel_title_from_prompt(prompt) else {
            return;
        };
        let id = self.durable.channels[index].id;
        Arc::make_mut(&mut self.durable).channels[index].title = title.clone();
        self.publish_model_edit(ModelEdit::RetitleChannel { id, title });
        self.refresh_sidebar_channels();
        self.persist();
    }

    /// Refresh cached rows after state changes, never during rendering.
    pub(super) fn refresh_sidebar_channels(&mut self) {
        let durable = &self.durable;
        let state = &self.sidebar_state;
        for project in &mut self.sidebar_model.projects {
            let mut index = 0;
            // Built in the durable vector's own order, which is the registry's
            // ascending creation ranks. Display order is not decided here:
            // `sidebar::channels_in_display_order` owns it, one named ordering
            // helper per row kind beside the one the worktree rows use.
            for channel in durable
                .channels
                .iter()
                .filter(|channel| channel.project_id == project.id)
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
                    if matches!(&card.kind, CardKind::Chat(_))
                        || matches!(&card.kind, CardKind::Terminal(session)
                            if crate::agent::Provider::recognize_launch(&session.launch).is_some())
                        || state.card_activity(*id).is_some()
                    {
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
                        && row.ordinal == channel.ordinal
                });
                if !unchanged {
                    let row = SidebarChannel::new(
                        channel.id,
                        title.to_owned(),
                        selected,
                        activity,
                        unread,
                        chats,
                        channel.ordinal,
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

const MAX_CHANNEL_TITLE_CHARS: usize = 80;

fn channel_title_from_prompt(prompt: &str) -> Option<String> {
    let title = normalize_card_name(prompt)?;
    Some(title.chars().take(MAX_CHANNEL_TITLE_CHARS).collect())
}

/// A channel still waiting for its first accepted prompt has to be nameable
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

    #[test]
    fn deleting_a_channel_closes_paired_sources_once_and_skips_exited_terminals() {
        use crate::model::{TerminalContinuity, TerminalExit};
        let app = super::super::tests::context_inspector_test_app(true);
        let CardKind::Terminal(source) = &app.durable.cards[0].kind else {
            panic!("fixture must have a terminal");
        };
        let mut chat = chat_session();
        chat.launch.provider = crate::agent::Provider::Codex;
        chat.terminal = Some(source.clone());
        let cards = vec![
            CardKind::Chat(chat.clone()),
            CardKind::Terminal(source.clone()),
        ];
        let ids = channel_terminal_ids(&cards, &[chat.clone()]);
        assert_eq!(ids, HashSet::from([source.id]));
        assert!(channel_terminal_needs_close(source, &ids));
        // The retained pairing is still needed after the chat actor disappears.
        assert_eq!(channel_terminal_ids(&cards[..1], &[]), ids);
        // The current descriptor, not a stale saved pairing, owns the source.
        let mut live = chat;
        let paired = live.terminal.as_mut().unwrap();
        paired.id = crate::model::TerminalId::new();
        let current = paired.id;
        assert_eq!(
            channel_terminal_ids(&cards[..1], &[live]),
            HashSet::from([current])
        );
        let exit = TerminalExit {
            code: Some(0),
            signal: None,
            occurred_at_unix_ms: 0,
        };
        let mut ended = source.clone();
        ended.continuity = TerminalContinuity::Exited {
            last_exit: exit.clone(),
        };
        assert!(!channel_terminal_needs_close(&ended, &ids));
        ended.continuity = TerminalContinuity::RelaunchAvailable {
            recipe: ended.launch.clone(),
            last_exit: Some(exit),
        };
        assert!(!channel_terminal_needs_close(&ended, &ids));
        assert!(!channel_terminal_needs_close(source, &HashSet::new()));
    }

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
        let rows = crate::sidebar::channels_in_display_order(
            app.sidebar_model.projects[0].channels.iter(),
        );
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].id, second, "the newest channel is the top row");
        assert_eq!(rows[1].id, first);
        let _ = app.handle_channel_ui(Action::Create(project));
        let third = app.durable.active_channel.unwrap();
        assert_ne!(third, second);
        assert_eq!(app.durable.channels.len(), 3);
        assert!(
            app.durable
                .channels
                .iter()
                .all(|channel| channel.cards.len() == 1)
        );
        assert!(
            !app.modal_is_open(),
            "creating a channel opens its composer directly"
        );
    }

    #[test]
    fn channel_uses_first_prompt_and_preserves_saved_names() {
        let mut app = super::super::tests::context_inspector_test_app(true);
        let card = app.workspace.focused().unwrap();
        let project = ProjectId::new();
        let id = ChannelId::new();
        let durable = Arc::make_mut(&mut app.durable);
        durable.cards[0].title = "Native chat".into();
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
            "New channel"
        );

        let prompt = format!("Explain the parser {}", "with detailed context ".repeat(12));
        let expected = channel_title_from_prompt(&prompt).expect("prompt has a title");
        app.name_channel_from_prompt(card, &prompt);
        assert_eq!(app.durable.channels[0].title, expected);
        assert_eq!(app.sidebar_model.projects[0].channels[0].title, expected);

        let durable = Arc::make_mut(&mut app.durable);
        durable.cards[0].title = "A later conversation title".into();
        app.name_channel_from_prompt(card, "A later prompt must not replace it");
        app.refresh_sidebar_channels();
        assert_eq!(app.sidebar_model.projects[0].channels[0].title, expected);
        assert_eq!(app.durable.channels[0].title, expected);
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

    #[test]
    fn empty_channels_delete_without_confirmation_even_with_closed_cards() {
        for closed_card in [false, true] {
            let mut app = super::super::tests::context_inspector_test_app(true);
            let id = ChannelId::new();
            let durable = Arc::make_mut(&mut app.durable);
            let cards = if closed_card {
                let mut card = durable.cards[0].clone();
                card.id = CardId::new();
                card.kind = CardKind::Chat(chat_session());
                card.existence = super::super::project_ui::removed_existence("closed fixture");
                let ids = vec![card.id];
                durable.cards.push(card);
                ids
            } else {
                Vec::new()
            };
            durable.channels.push(ProjectChannel {
                id,
                project_id: ProjectId::new(),
                title: "Empty".into(),
                ordinal: 0,
                shared: false,
                cards,
                focused: None,
            });
            let _ = app.handle_channel_ui(Action::Delete(id));
            assert!(!app.durable.channels.iter().any(|channel| channel.id == id));
            assert!(!app.channel_ui.modal_open());
        }
    }

    #[test]
    fn a_channel_with_a_cli_terminal_still_requires_confirmation() {
        let mut app = super::super::tests::context_inspector_test_app(true);
        let id = ChannelId::new();
        let card = app.durable.cards[0].id;
        assert!(matches!(app.durable.cards[0].kind, CardKind::Terminal(_)));
        Arc::make_mut(&mut app.durable)
            .channels
            .push(ProjectChannel {
                id,
                project_id: ProjectId::new(),
                title: "CLI".into(),
                ordinal: 0,
                shared: false,
                cards: vec![card],
                focused: Some(card),
            });
        let _ = app.handle_channel_ui(Action::Delete(id));
        assert_eq!(app.channel_ui.dialog.as_ref().unwrap().chats, 1);
        assert!(app.durable.channels.iter().any(|channel| channel.id == id));
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
        assert!(app.channel_ui.is_deleting(doomed));
        let _ = app.handle_channel_ui(Action::SessionsClosed(doomed, Ok(())));
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
