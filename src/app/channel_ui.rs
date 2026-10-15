use super::*;
use crate::agent::ActivityState;
use crate::model::{ChannelId, ProjectChannel};
use crate::sidebar::SidebarChannel;
use crate::workspace_model::ModelEdit;

/// Closes the daemon's live sessions behind `sessions`, by the identities the
/// daemon itself reports rather than the saved descriptors, together with the
/// source terminal a converted chat keeps running.
pub(super) fn close_sessions(sessions: Vec<CardKind>) -> Result<(), String> {
    if !sessions
        .iter()
        .any(|kind| matches!(kind, CardKind::Terminal(_) | CardKind::Chat(_)))
    {
        return Ok(());
    }
    let socket = crate::terminal::ipc::daemon_socket_path().map_err(|error| error.to_string())?;
    let Some(mut client) = connect(&socket)? else {
        return no_daemon_close_result(&sessions);
    };
    // Query live identities: a saved descriptor can predate an upgrade or handoff.
    let terminals = client.list().map_err(|error| error.to_string())?;
    let chats = client.list_chats().map_err(|error| error.to_string())?;
    let terminal_ids = channel_terminal_ids(&sessions, &chats);
    for kind in &sessions {
        if let CardKind::Chat(session) = kind {
            let Some(live) = chats.iter().find(|live| live.id == session.id) else {
                if may_have_retained_owner(session) {
                    client
                        .close_retained_chat(session.clone())
                        .map_err(|error| error.to_string())?;
                }
                continue;
            };
            if let Err(error) = client.close_chat(live.clone()) {
                // Another window or a natural exit can win this race.
                // Only a fresh inventory may turn a close error into success,
                // read on a fresh connection: a failed request can leave this
                // one out of step with the daemon.
                let Some(fresh) = connect(&socket)? else {
                    return if may_have_retained_owner(live) {
                        Err(error.to_string())
                    } else {
                        no_daemon_close_result(&sessions)
                    };
                };
                client = fresh;
                if client
                    .list_chats()
                    .map_err(|error| error.to_string())?
                    .iter()
                    .any(|chat| chat.id == live.id)
                {
                    return Err(error.to_string());
                }
                if may_have_retained_owner(live) {
                    // Disappearance from actor inventory can mean retained
                    // ownership, not process exit. Demand its close receipt.
                    client
                        .close_retained_chat(live.clone())
                        .map_err(|error| error.to_string())?;
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
            let Some(fresh) = connect(&socket)? else {
                return no_daemon_close_result(&sessions);
            };
            client = fresh;
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

fn may_have_retained_owner(session: &crate::chat::Session) -> bool {
    session.owned_codex_listener.is_some()
        || matches!(
            session.native_continuity,
            crate::chat::NativeContinuity::Durable { .. }
        )
}

fn no_daemon_close_result(sessions: &[CardKind]) -> Result<(), String> {
    if sessions
        .iter()
        .any(|kind| matches!(kind, CardKind::Chat(session) if may_have_retained_owner(session)))
    {
        Err("Reconnect the daemon to confirm the retained native provider has stopped".into())
    } else {
        Ok(())
    }
}

/// A connection to the daemon, or `None` when no daemon is running. Retained
/// native owners can outlive it, so callers must not treat this as their exit.
fn connect(socket: &std::path::Path) -> Result<Option<crate::control::ControlClient>, String> {
    match crate::control::ControlClient::connect(socket) {
        Ok(client) => Ok(Some(client)),
        Err(crate::control::ControlError::Connect(error))
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) =>
        {
            Ok(None)
        }
        Err(error) => Err(format!(
            "Cannot reach the daemon to close sessions: {error}"
        )),
    }
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
    /// Always asks first, even for an empty channel: a stray keystroke on a
    /// focused row must never delete it outright.
    RequestDelete(ChannelId),
    ConfirmDelete,
    Dismiss,
    SessionsClosed(ChannelId, Result<(), String>),
    RetryDelete(ChannelId),
    /// Runs a deletion that failed again, from the channel's row.
    RetryFailed(ChannelId),
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
    /// Deletions under way. Each runs behind its channel's row rather than a
    /// dialog, so the rest of the window stays usable meanwhile.
    deleting: HashSet<ChannelId>,
    /// Deletions that failed, with why, until retried or declined.
    failed: HashMap<ChannelId, String>,
    /// Channels this window has deleted.
    ///
    /// A window is not sent the echo of its own edits, so its cached copy of
    /// the shared model keeps naming a channel it deleted until the next full
    /// read, and a read can be older than the deletion. Either would adopt the
    /// channel straight back. An identity is never reused, so remembering it
    /// for the life of the window is exact.
    deleted: HashSet<ChannelId>,
}

#[derive(Debug, Clone)]
struct Deletion {
    channel: ChannelId,
    title: String,
    /// Open conversations the deletion closes, reported before it happens.
    chats: usize,
    /// Why the last deletion of this channel failed, when the dialog is
    /// opened again after it. Confirming retries it.
    failure: Option<String>,
}

impl State {
    pub fn modal_open(&self) -> bool {
        self.dialog.is_some()
    }

    fn row_deletion(&self, id: ChannelId) -> Option<crate::sidebar::RowDeletion> {
        if self.deleting.contains(&id) {
            return Some(crate::sidebar::RowDeletion::Deleting);
        }
        self.failed
            .get(&id)
            .map(|reason| crate::sidebar::RowDeletion::Failed(reason.as_str().into()))
    }

    pub(super) fn is_deleting(&self, id: ChannelId) -> bool {
        self.deleting.contains(&id)
    }

    /// Whether any channel deletion is under way.
    pub(super) fn deletion_running(&self) -> bool {
        !self.deleting.is_empty()
    }

    pub(super) fn was_deleted(&self, id: ChannelId) -> bool {
        self.deleted.contains(&id)
    }

    pub(super) fn focus_target() -> widget::Id {
        widget::Id::new("dialog.channel.confirm")
    }
}

impl Cagents {
    fn open_channel_with_default(&mut self, id: ChannelId) -> Task<Message> {
        let scope = self.select_channel(id);
        if self.durable.active_channel != Some(id) {
            return scope;
        }
        Task::batch([scope, self.ensure_default_pane()])
    }

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
                    let title = self
                        .durable
                        .channels
                        .iter()
                        .find(|channel| channel.id == id)
                        .map_or("the channel", channel_display_title);
                    self.status = Some(TransientNotice::error(format!(
                        "Could not delete {title}: {error}"
                    )));
                    self.channel_ui.failed.insert(id, error);
                    self.refresh_sidebar_channels();
                    Task::none()
                }
            };
        }
        if self.update_prepared
            || self.focused_repository_is_saving()
            || (self.modal_is_open() && !action.answers_own_dialog())
        {
            return Task::none();
        }
        match action {
            Action::SessionsClosed(..) | Action::RetryDelete(_) => unreachable!(),
            Action::RetryFailed(id) => {
                if !self.channel_ui.failed.contains_key(&id) {
                    return Task::none();
                }
                self.delete_channel(id)
            }
            Action::Create(project_id) => {
                let Some(project) = self
                    .sidebar_model
                    .projects
                    .iter()
                    .find(|project| project.id == project_id)
                else {
                    return Task::none();
                };
                if self.project_ui.removing.contains_key(&project.repository)
                    || self.project_ui.closed.contains(&project.repository)
                {
                    self.status = Some(TransientNotice::warning("That project is being removed."));
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
                        title_source: Default::default(),
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
                let scope = self.open_channel_with_default(id);
                self.persist();
                scope
            }
            // A channel being deleted is not somewhere to go; its row offers
            // nothing, and this refuses anything that raced it.
            Action::Open(id) if self.channel_ui.is_deleting(id) => Task::none(),
            Action::Open(id) => self.open_channel_with_default(id),
            Action::Delete(id) => self.begin_channel_deletion(id, false),
            Action::RequestDelete(id) => self.begin_channel_deletion(id, true),
            Action::ConfirmDelete => {
                let Some(deletion) = self.channel_ui.dialog.take() else {
                    return Task::none();
                };
                self.delete_channel(deletion.channel)
            }
            Action::Dismiss => {
                // Declining to delete a channel whose deletion failed is the
                // answer to that failure, so its row stops asking.
                if let Some(dismissed) = self.channel_ui.dialog.take()
                    && self.channel_ui.failed.remove(&dismissed.channel).is_some()
                {
                    self.refresh_sidebar_channels();
                }
                let focus = self.nested_modal_focus_target().unwrap_or_else(|| {
                    self.workspace
                        .focused()
                        .map(|card| self.card_keyboard_focus(card))
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
        let emptied = self.channels_emptied_by(retired);
        self.delete_channels(emptied)
    }

    /// The channels `retired` would leave without an open card.
    ///
    /// Channel membership is pruned of closed cards whenever the scope is
    /// persisted, so this must be read before the retirement is persisted;
    /// afterwards the channel no longer records that it held these cards.
    ///
    /// Only the channel's own content counts. A utility pane such as Settings
    /// belongs to the window and merely follows the user into whichever
    /// channel they are viewing, so closing it never empties a channel - the
    /// channel is shared with every window, whose conversations it would end
    /// - and it does not keep an otherwise empty channel alive either.
    pub(super) fn channels_emptied_by(&self, retired: &[CardId]) -> Vec<ChannelId> {
        self.durable
            .channels
            .iter()
            .filter(|channel| {
                channel
                    .cards
                    .iter()
                    .any(|&card| retired.contains(&card) && self.is_channel_content(card))
            })
            .filter(|channel| {
                !channel.cards.iter().any(|&card| {
                    !retired.contains(&card)
                        && self.is_channel_content(card)
                        && self
                            .card_state(card)
                            .is_some_and(|state| state.existence.is_open())
                })
            })
            .map(|channel| channel.id)
            .collect()
    }

    pub(super) fn channel_has_open_card(&self, id: ChannelId) -> bool {
        self.durable
            .channels
            .iter()
            .find(|channel| channel.id == id)
            .is_some_and(|channel| {
                channel.cards.iter().any(|&card| {
                    self.card_state(card)
                        .is_some_and(|state| state.existence.is_open() && !state.kind.is_utility())
                })
            })
    }

    /// Whether `card` is part of what a channel holds, rather than a window
    /// utility pane that is only passing through it.
    fn is_channel_content(&self, card: CardId) -> bool {
        self.card_state(card)
            .is_none_or(|state| !state.kind.is_utility())
    }

    /// Opens a channel's delete confirmation, or deletes an empty channel at
    /// once unless `always_confirm`.
    fn begin_channel_deletion(&mut self, id: ChannelId, always_confirm: bool) -> Task<Message> {
        if self.channel_ui.is_deleting(id) {
            return Task::none();
        }
        let Some(channel) = self
            .durable
            .channels
            .iter()
            .find(|channel| channel.id == id)
        else {
            return Task::none();
        };
        let title = channel_display_title(channel).to_owned();
        let chats = self.channel_conversation_count(channel);
        if chats == 0 && !always_confirm {
            return self.delete_channel(id);
        }
        self.channel_ui.dialog = Some(Deletion {
            channel: id,
            title,
            chats,
            failure: self.channel_ui.failed.get(&id).cloned(),
        });
        iced::widget::operation::focus(State::focus_target()).map(cosmic::Action::App)
    }

    /// The open conversations - chats and terminals - that deleting
    /// `channel` closes with it.
    pub(super) fn channel_conversation_count(&self, channel: &ProjectChannel) -> usize {
        channel
            .cards
            .iter()
            .filter(|card| {
                self.durable.cards.iter().any(|state| {
                    state.id == **card
                        && state.existence.is_open()
                        && matches!(&state.kind, CardKind::Chat(_) | CardKind::Terminal(_))
                })
            })
            .count()
    }

    pub(super) fn delete_channels(&mut self, ids: Vec<ChannelId>) -> Task<Message> {
        Task::batch(ids.into_iter().map(|id| self.delete_channel(id)))
    }

    /// Removes a channel along with everything that existed only inside it.
    ///
    /// A channel's conversations are reachable nowhere else, so they are
    /// retired with it rather than left behind as durable cards no scope can
    /// show - the same treatment worktree deletion gives the cards of the
    /// directory it removes.
    pub(super) fn delete_channel(&mut self, id: ChannelId) -> Task<Message> {
        let Some(cards) = self
            .durable
            .channels
            .iter()
            .find(|channel| channel.id == id)
            .map(|channel| channel.cards.clone())
        else {
            return Task::none();
        };
        if !self.channel_ui.deleting.insert(id) {
            return Task::none();
        }
        self.channel_ui.failed.remove(&id);
        self.refresh_sidebar_channels();
        // Settling spawns and closing sessions take a round trip each, and the
        // row reports them meanwhile. The workspace leaves the channel now, so
        // nothing the user does in the meantime lands in a channel whose
        // sessions are being closed.
        let leave = if self.durable.active_channel == Some(id)
            && let Some(index) = self
                .durable
                .channels
                .iter()
                .position(|channel| channel.id == id)
        {
            let (project_id, successor) = self.channel_successor(index);
            self.land_after_channel(project_id, successor)
        } else {
            Task::none()
        };
        if cards
            .iter()
            .any(|card| self.pending_terminal_spawn(*card) || self.chat_start_pending(*card))
        {
            let retry = cosmic::task::future(async move {
                tokio::time::sleep(Duration::from_millis(50)).await;
                Message::ChannelUi(Action::RetryDelete(id))
            });
            return Task::batch([leave, retry]);
        }
        let sessions: Vec<_> = self
            .durable
            .cards
            .iter()
            .filter(|card| cards.contains(&card.id) && card.existence.is_open())
            .map(|card| card.kind.clone())
            .collect();
        if !sessions
            .iter()
            .any(|kind| matches!(kind, CardKind::Terminal(_) | CardKind::Chat(_)))
        {
            self.channel_ui.deleting.remove(&id);
            return Task::batch([leave, self.finish_delete_channel(id)]);
        }
        let close = cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || close_sessions(sessions))
                .await
                .map_err(|error| error.to_string())
                .and_then(|result| result);
            Message::ChannelUi(Action::SessionsClosed(id, result))
        });
        Task::batch([leave, close])
    }

    pub(super) fn finish_delete_channel(&mut self, id: ChannelId) -> Task<Message> {
        self.retire_channel(id, false)
    }

    pub(super) fn retire_peer_channel(&mut self, id: ChannelId) -> Task<Message> {
        self.retire_channel(id, true)
    }

    fn retire_channel(&mut self, id: ChannelId, passive: bool) -> Task<Message> {
        let Some(index) = self
            .durable
            .channels
            .iter()
            .position(|channel| channel.id == id)
        else {
            return Task::none();
        };
        let was_active = self.durable.active_channel == Some(id);
        let (project_id, successor) = self.channel_successor(index);
        let fallback = self.project_primary_worktree(project_id);
        let focused = self.durable.channels[index].focused;
        let mut retained_scope = None;
        let channel = Arc::make_mut(&mut self.durable).channels.remove(index);
        // Every window loses the channel, not only this one. A deletion that
        // arrived from the registry publishes it right back, which the registry
        // answers as a no-op because the channel is already gone - cheaper than
        // threading an origin flag through the one path that must behave
        // identically whoever asked.
        self.publish_model_edit(ModelEdit::DeleteChannel { id });
        self.channel_ui.deleted.insert(id);
        let mut freed_utility = false;
        for card in channel.cards {
            // A utility pane outlives the channel it was visiting: it leaves
            // with no channel, which every worktree strip shows, and opening
            // it again from a channel brings it there.
            if !self.is_channel_content(card) {
                freed_utility = true;
                continue;
            }
            if passive
                && (self.shared_chat_has_local_work(card)
                    || matches!(self.cards.get(&card), Some(CardRuntime::Repository(runtime)) if runtime.is_dirty() || runtime.is_saving()))
            {
                self.retain_local_pane(card);
                let scope = self
                    .durable
                    .card_scopes
                    .get(&card)
                    .cloned()
                    .or_else(|| self.worktree_owners.get(&card).cloned())
                    .or_else(|| {
                        self.card_state(card)
                            .and_then(|state| self.card_worktree(state))
                            .map(Path::to_path_buf)
                    })
                    .or_else(|| fallback.clone());
                if let Some(scope) = scope {
                    Arc::make_mut(&mut self.durable)
                        .card_scopes
                        .insert(card, scope.clone());
                    self.worktree_owners.insert(card, scope.clone());
                    if focused == Some(card) || retained_scope.is_none() {
                        retained_scope = Some(scope.clone());
                        self.worktree_focus.insert(scope, card);
                    }
                }
                continue;
            }
            self.retire_lsp_card(card);
            self.sidebar_state.clear_agent_activity(card);
            self.sidebar_state.acknowledge_agent_update(card);
            if passive {
                self.shared_chat_detach(card);
            } else {
                self.chat_discard(card);
            }
            self.cards.remove(&card);
            self.lifecycle.remove(card);
            if let Some(state) = self.card_state_mut(card) {
                state.existence = super::project_ui::removed_existence("channel deleted by user");
            }
        }
        if retained_scope.is_some() {
            self.status = Some(TransientNotice::warning(
                "A channel was deleted in another client. Your unsaved work is retained in its worktree.",
            ));
        }
        let select = if was_active {
            if let Some(scope) = retained_scope {
                self.select_worktree(scope, false)
            } else {
                self.land_after_channel(project_id, successor)
            }
        } else if freed_utility
            && self.durable.active_channel.is_none()
            && let Some(path) = self.active_workspace.clone()
        {
            // The freed pane now belongs in the worktree strip on screen,
            // which was built without it.
            self.select_worktree(path, false)
        } else {
            Task::none()
        };
        // After the reselection: leaving a scope records its camera against
        // the channel it is leaving, including this one.
        self.channel_offsets.remove(&id);
        self.refresh_sidebar_channels();
        self.persist();
        Task::batch([
            select,
            self.lifecycle_sweep(),
            self.sync_chrome(),
            self.complete_closed_projects(),
        ])
    }

    /// The project of the channel at `index`, and the channel the workspace
    /// moves to once it is gone: the one below it in the sidebar, or the one
    /// above it when the project's oldest channel is the one going away. Below
    /// means the next lower rank, because the sidebar renders newest first.
    fn channel_successor(&self, index: usize) -> (ProjectId, Option<ChannelId>) {
        let leaving = &self.durable.channels[index];
        let mut below = None::<&ProjectChannel>;
        let mut above = None::<&ProjectChannel>;
        for channel in self.durable.channels.iter().filter(|channel| {
            channel.id != leaving.id
                && channel.project_id == leaving.project_id
                && !self.channel_ui.is_deleting(channel.id)
        }) {
            if channel.ordinal <= leaving.ordinal {
                if below.is_none_or(|best| channel.ordinal > best.ordinal) {
                    below = Some(channel);
                }
            } else if above.is_none_or(|best| channel.ordinal < best.ordinal) {
                above = Some(channel);
            }
        }
        (
            leaving.project_id,
            below.or(above).map(|channel| channel.id),
        )
    }

    /// A scope that is going away cannot stay selected, and the workspace has
    /// to land on something the user can still reach.
    fn land_after_channel(
        &mut self,
        project_id: ProjectId,
        successor: Option<ChannelId>,
    ) -> Task<Message> {
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
        let mut rows: Vec<Element<'_, Message>> = vec![
            design::title("Delete channel?").into(),
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
        ];
        if let Some(failure) = &dialog.failure {
            rows.push(super::notice_body(format!(
                "The last attempt failed: {failure}"
            )));
        }
        rows.push(super::dialog_ui::confirmation_actions(
            crate::ui_controls::button::secondary("Cancel")
                .id(widget::Id::new("dialog.channel.close"))
                .on_press(Message::ChannelUi(Action::Dismiss)),
            crate::ui_controls::button::destructive(if dialog.failure.is_some() {
                "Retry delete"
            } else {
                "Delete channel"
            })
            .id(State::focus_target())
            .on_press(Message::ChannelUi(Action::ConfirmDelete)),
        ));
        Some(super::dialog_ui::surface(
            widget::column::with_children(rows).spacing(space::M),
            480.0,
        ))
    }
    pub(super) fn channel_needs_name(&self, card: CardId) -> bool {
        self.durable.channels.iter().any(|channel| {
            (channel.title.is_empty()
                || channel.title_source == crate::model::ChannelTitleSource::Prompt)
                && channel.cards.contains(&card)
        })
    }

    /// Names a new channel from its first submitted prompt or conversation name.
    ///
    /// Prompt fallbacks may later improve to provider titles. Manual and legacy
    /// titles remain authoritative, including renames from another window.
    pub(super) fn name_channel_from_prompt(&mut self, card: CardId, prompt: &str) {
        if let Some(title) = channel_title_from_prompt(prompt) {
            self.suggest_channel_title(card, title, crate::model::ChannelTitleSource::Prompt);
        }
    }

    pub(super) fn suggest_channel_title(
        &mut self,
        card: CardId,
        title: String,
        source: crate::model::ChannelTitleSource,
    ) {
        let Some(index) = self.durable.channels.iter().position(|channel| {
            channel.title_source.accepts(&channel.title, source) && channel.cards.contains(&card)
        }) else {
            return;
        };
        let id = self.durable.channels[index].id;
        Arc::make_mut(&mut self.durable).channels[index].title = title.clone();
        Arc::make_mut(&mut self.durable).channels[index].title_source = source;
        self.publish_model_edit(ModelEdit::SuggestChannelTitle { id, title, source });
        self.refresh_sidebar_channels();
        self.persist();
    }

    /// Refresh cached rows after state changes, never during rendering.
    pub(super) fn refresh_sidebar_channels(&mut self) {
        let durable = &self.durable;
        // A failure outlives nothing: a channel that has gone since, however
        // it went, has nothing left to retry.
        self.channel_ui
            .failed
            .retain(|id, _| durable.channels.iter().any(|channel| channel.id == *id));
        let channel_ui = &self.channel_ui;
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
                let title = if channel.title.is_empty() {
                    let branch = project
                        .worktrees
                        .iter()
                        .find(|worktree| {
                            sidebar::is_primary_worktree(&project.repository, &worktree.path)
                        })
                        .and_then(|worktree| worktree.branch.as_deref());
                    new_channel_title(branch)
                } else {
                    channel_display_title(channel).to_owned()
                };
                let selected = durable.active_channel == Some(channel.id);
                let deletion = channel_ui.row_deletion(channel.id);
                let mut activity = None;
                let mut unread = false;
                let mut chats = 0;
                let mut has_panes = false;
                for id in &channel.cards {
                    // A visiting utility pane is not what the channel holds.
                    let Some(card) = durable.cards.iter().find(|card| {
                        card.id == *id && card.existence.is_open() && !card.kind.is_utility()
                    }) else {
                        continue;
                    };
                    has_panes = true;
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
                        && row.has_panes == has_panes
                        && row.ordinal == channel.ordinal
                        && row.deletion == deletion
                });
                if !unchanged {
                    let mut row = SidebarChannel::new(
                        channel.id,
                        title.to_owned(),
                        selected,
                        activity,
                        unread,
                        chats,
                        has_panes,
                        channel.ordinal,
                    );
                    row.deletion = deletion;
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

fn channel_title_from_prompt(prompt: &str) -> Option<String> {
    crate::workspace_model::channel_title_from_prompt(prompt)
}

fn new_channel_title(branch: Option<&str>) -> String {
    match branch {
        Some(branch) => format!(
            "New channel · {}",
            branch.strip_prefix("refs/heads/").unwrap_or(branch)
        ),
        None => "New channel".into(),
    }
}

/// A channel still waiting for its first accepted prompt has to be nameable
/// anyway: a row has to label it, and a dialog about to destroy it has to say
/// which one it means.
pub(super) fn channel_display_title(channel: &ProjectChannel) -> &str {
    if channel.title.is_empty() {
        "New channel"
    } else {
        &channel.title
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sidebar::RowDeletion;

    #[test]
    fn new_channel_titles_name_the_branch() {
        assert_eq!(
            new_channel_title(Some("feature/login")),
            "New channel · feature/login"
        );
        assert_eq!(
            new_channel_title(Some("refs/heads/main")),
            "New channel · main"
        );
        assert_eq!(new_channel_title(None), "New channel");
    }

    #[test]
    fn missing_daemon_is_not_close_proof_for_a_retained_native_card() {
        let mut session = chat_session();
        assert!(no_daemon_close_result(&[CardKind::Chat(session.clone())]).is_ok());
        session.launch.provider = crate::agent::Provider::Codex;
        session.native_continuity = crate::chat::NativeContinuity::Durable {
            provider_version: "0.157.1".into(),
        };
        assert!(no_daemon_close_result(&[CardKind::Chat(session)]).is_err());
        let mut unnamed = chat_session();
        unnamed.launch.provider = crate::agent::Provider::Codex;
        unnamed.owned_codex_listener = Some(crate::chat::OwnedCodexListener {
            socket: "/owned/control.sock".into(),
            allocator: crate::model::HostSessionProof {
                host_session_id: crate::model::HostSessionId::new("owner"),
                host_incarnation: crate::model::HostIncarnation::new("incarnation"),
                boot_id: "boot".into(),
            },
        });
        assert!(no_daemon_close_result(&[CardKind::Chat(unnamed)]).is_err());
    }

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
        assert_eq!(app.active_workspace.as_deref(), Some(Path::new("/repo")));
        app.refresh_sidebar_channels();
        assert_eq!(
            app.sidebar_model.projects[0].channels[0].title,
            "New channel · main"
        );
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
                .all(|channel| { channel.cards.len() == usize::from(channel.id == third) }),
            "only the active channel retains its unused default pane"
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
            title_source: Default::default(),
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
            native_continuity: Default::default(),
            owned_codex_listener: None,
            id: crate::model::TerminalId::new(),
            launch: crate::chat::Launch {
                provider: crate::agent::Provider::Omp,
                recipe: crate::model::LaunchRecipe {
                    unset_environment: Default::default(),
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
            title_source: Default::default(),
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

    /// Any open card makes a channel somewhere the user has work, a plain
    /// shell included, even though only conversations count as its chats.
    /// Once the last one closes the row has nothing open and mutes.
    #[test]
    fn a_channel_row_holds_panes_while_any_card_in_it_is_open() {
        let mut app = super::super::tests::context_inspector_test_app(true);
        let project = ProjectId::new();
        let shell = app.durable.cards[0].id;
        Arc::make_mut(&mut app.durable)
            .channels
            .push(ProjectChannel {
                title_source: Default::default(),
                id: ChannelId::new(),
                project_id: project,
                title: "Investigation".into(),
                ordinal: 0,
                shared: false,
                cards: vec![shell],
                focused: Some(shell),
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
        let row = &app.sidebar_model.projects[0].channels[0];
        assert_eq!(row.chats, 0, "a shell is not a conversation");
        assert!(row.has_panes, "but it is an open pane");

        Arc::make_mut(&mut app.durable).cards[0].existence =
            super::super::project_ui::removed_existence("closed");
        app.refresh_sidebar_channels();
        assert!(!app.sidebar_model.projects[0].channels[0].has_panes);
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
                title_source: Default::default(),
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
                title_source: Default::default(),
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

    /// A window whose active channel, "Investigation", holds a live
    /// conversation, beside an empty "Migration": (window, doomed, survivor,
    /// conversation).
    fn channel_with_a_conversation() -> (Cagents, ChannelId, ChannelId, CardId) {
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
            title_source: Default::default(),
            id: doomed,
            project_id: project,
            title: "Investigation".into(),
            ordinal: 0,
            shared: false,
            cards: vec![conversation],
            focused: Some(conversation),
        });
        durable.channels.push(ProjectChannel {
            title_source: Default::default(),
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
        (app, doomed, survivor, conversation)
    }

    /// A channel's conversations are reachable nowhere else, so deleting one
    /// has to take them with it, say so first, and leave the workspace on a
    /// scope that still exists.
    #[test]
    fn deleting_the_active_channel_retires_its_chats_and_lands_on_a_survivor() {
        let (mut app, doomed, survivor, conversation) = channel_with_a_conversation();
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

    /// Confirming a deletion lets the window go on while the channel's
    /// sessions close (#264): the confirmation is gone, the workspace has
    /// already left the channel, and its row says what is happening. A
    /// failure stays on the row, which runs the deletion again.
    #[test]
    fn a_channel_deletion_runs_behind_its_row_and_a_failure_is_retried_from_there() {
        let (mut app, doomed, survivor, _) = channel_with_a_conversation();
        let deletion = |app: &Cagents| {
            app.sidebar_model.projects[0]
                .channels
                .iter()
                .find(|row| row.id == doomed)
                .unwrap()
                .deletion
                .clone()
        };

        let _ = app.handle_channel_ui(Action::Delete(doomed));
        let _ = app.handle_channel_ui(Action::ConfirmDelete);
        assert!(!app.modal_is_open() && app.dialog().is_none());
        assert_eq!(deletion(&app), Some(RowDeletion::Deleting));
        assert_eq!(app.durable.active_channel, Some(survivor));

        // Nothing reopens or deletes it twice while its sessions close.
        let _ = app.handle_channel_ui(Action::Open(doomed));
        assert_eq!(app.durable.active_channel, Some(survivor));
        let _ = app.handle_channel_ui(Action::Delete(doomed));
        assert!(app.dialog().is_none());
        let _ = app.handle_channel_ui(Action::RetryFailed(doomed));
        assert!(app.channel_ui.is_deleting(doomed));

        let _ = app.handle_channel_ui(Action::SessionsClosed(
            doomed,
            Err("the daemon is unreachable".into()),
        ));
        assert!(!app.channel_ui.is_deleting(doomed));
        assert_eq!(
            deletion(&app),
            Some(RowDeletion::Failed("the daemon is unreachable".into()))
        );
        assert_eq!(
            app.status.as_ref().map(|notice| notice.message.as_str()),
            Some("Could not delete Investigation: the daemon is unreachable")
        );
        assert!(app.durable.channels.iter().any(|row| row.id == doomed));

        let _ = app.handle_channel_ui(Action::RetryFailed(doomed));
        assert_eq!(deletion(&app), Some(RowDeletion::Deleting));
        let _ = app.handle_channel_ui(Action::SessionsClosed(
            doomed,
            Err("the daemon is still unreachable".into()),
        ));

        // Asked to delete it again, the dialog says why the last attempt
        // failed, and confirming there retries it.
        let _ = app.handle_channel_ui(Action::Delete(doomed));
        assert_eq!(
            app.channel_ui
                .dialog
                .as_ref()
                .and_then(|dialog| dialog.failure.as_deref()),
            Some("the daemon is still unreachable")
        );
        let _ = app.handle_channel_ui(Action::ConfirmDelete);
        assert_eq!(deletion(&app), Some(RowDeletion::Deleting));
        let _ = app.handle_channel_ui(Action::SessionsClosed(doomed, Ok(())));
        assert!(!app.durable.channels.iter().any(|row| row.id == doomed));
    }

    #[test]
    fn a_visiting_settings_pane_is_not_what_a_channel_holds() {
        let mut app = super::super::tests::context_inspector_test_app(true);
        let channel = ChannelId::new();
        let conversation = CardId::new();
        let settings = CardId::new();
        let durable = Arc::make_mut(&mut app.durable);
        let mut chat = durable.cards[0].clone();
        chat.id = conversation;
        chat.kind = CardKind::Chat(chat_session());
        let mut pane = durable.cards[0].clone();
        pane.id = settings;
        pane.kind = CardKind::Settings;
        pane.title = "Settings".into();
        durable.cards.extend([chat, pane]);
        durable.channels.push(ProjectChannel {
            title_source: Default::default(),
            id: channel,
            project_id: ProjectId::new(),
            title: "Investigation".into(),
            ordinal: 0,
            shared: true,
            cards: vec![conversation, settings],
            focused: Some(settings),
        });
        app.cards.insert(settings, CardRuntime::Settings);

        // Closing the window's settings pane must not delete a channel that
        // every window shares, and with it their conversations.
        assert!(app.channels_emptied_by(&[settings]).is_empty());
        // Nor does it keep a channel whose conversations are all gone.
        assert_eq!(app.channels_emptied_by(&[conversation]), [channel]);
        assert!(app.channel_has_open_card(channel));
        Arc::make_mut(&mut app.durable).cards[1].existence =
            super::super::project_ui::removed_existence("closed fixture");
        app.sidebar_model.projects.push(SidebarProject::new(
            ProjectId::new(),
            RepositoryId::new(),
            "project".into(),
            "/repo".into(),
            Vec::new(),
            Vec::new(),
        ));
        Arc::make_mut(&mut app.durable).channels[0].project_id = app.sidebar_model.projects[0].id;
        app.refresh_sidebar_channels();
        assert!(
            !app.sidebar_model.projects[0].channels[0].has_panes,
            "the sidebar mutes a channel only settings is visiting"
        );
        Arc::make_mut(&mut app.durable).cards[1].existence =
            super::super::project_ui::removed_existence("closed fixture");
        assert!(!app.channel_has_open_card(channel));

        // When the channel goes, the pane outlives it rather than vanishing
        // from under the user; it is simply nobody's channel any more.
        let _ = app.handle_channel_ui(Action::Delete(channel));
        assert!(!app.durable.channels.iter().any(|row| row.id == channel));
        assert!(app.card_state(settings).unwrap().existence.is_open());
        assert!(matches!(
            app.cards.get(&settings),
            Some(CardRuntime::Settings)
        ));
    }
}
