use super::*;
use crate::model::{ChannelId, ProjectChannel};
use crate::sidebar::SidebarChannel;
use crate::workspace_model::ModelEdit;
use std::collections::BTreeMap;

/// Closes the daemon's live sessions behind `sessions`, by the identities the
/// daemon itself reports rather than the saved descriptors, together with the
/// source terminal a converted chat keeps running.
///
/// An agent that still runs as one of these conversations stays open: this
/// also gives up allocations nobody asked to close, such as a failed start.
pub(super) fn close_sessions(sessions: Vec<CardKind>) -> Result<(), String> {
    close_sessions_with(sessions, OpenAgents::Keep)
}

/// [`close_sessions`] for conversations the user closed, which retires an
/// open agent that still runs as one whose actor is gone. A daemon stop keeps
/// such agents open while their conversations come back without a live
/// actor, and closing the card is how the user ends them.
pub(super) fn close_sessions_retiring_agents(sessions: Vec<CardKind>) -> Result<(), String> {
    close_sessions_with(sessions, OpenAgents::Retire)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum OpenAgents {
    Keep,
    Retire,
}

fn close_sessions_with(sessions: Vec<CardKind>, open_agents: OpenAgents) -> Result<(), String> {
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
    // Read only once a conversation turns out not to be live, the state a
    // daemon stop leaves an open agent's conversation in.
    let mut agents = None;
    for kind in &sessions {
        if let CardKind::Chat(session) = kind {
            let Some(live) = chats.iter().find(|live| live.id == session.id) else {
                if may_have_retained_owner(session)
                    || (open_agents == OpenAgents::Retire
                        && bound_to_open_agent(
                            session,
                            read_open_agents(&mut agents, &mut client)?,
                        ))
                {
                    client
                        .close_retained_chat(session.clone())
                        .map_err(|error| error.reason())?;
                }
                continue;
            };
            let closed = client.close_chat(live.clone());
            if closed.is_ok() && open_agents == OpenAgents::Retire {
                // A reopened conversation that was given no work yet leaves
                // its agent stopped when it closes, for a later reopening to
                // take over. Closing it for good retires that agent too, read
                // afresh since the close may already have retired it.
                agents = None;
                if bound_to_open_agent(live, read_open_agents(&mut agents, &mut client)?) {
                    client
                        .close_retained_chat(live.clone())
                        .map_err(|error| error.reason())?;
                }
            }
            if let Err(error) = closed {
                // Another window or a natural exit can win this race.
                // Only a fresh inventory may turn a close error into success,
                // read on a fresh connection: a failed request can leave this
                // one out of step with the daemon.
                let Some(fresh) = connect(&socket)? else {
                    return if may_have_retained_owner(live) {
                        Err(error.reason())
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
                    return Err(error.reason());
                }
                if may_have_retained_owner(live)
                    || (open_agents == OpenAgents::Retire
                        && bound_to_open_agent(live, read_open_agents(&mut agents, &mut client)?))
                {
                    // Disappearance from actor inventory can mean retained
                    // ownership, not process exit. Demand its close receipt.
                    client
                        .close_retained_chat(live.clone())
                        .map_err(|error| error.reason())?;
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

/// The registry's open agents, read on first use.
fn read_open_agents<'a>(
    agents: &'a mut Option<Vec<crate::agent_execution::Agent>>,
    client: &mut crate::control::ControlClient,
) -> Result<&'a [crate::agent_execution::Agent], String> {
    if agents.is_none() {
        let model = client
            .read_workspace_model()
            .map_err(|error| error.to_string())?;
        *agents = Some(
            model
                .agents
                .into_iter()
                .filter(|agent| !agent.closed)
                .collect(),
        );
    }
    Ok(agents.as_deref().unwrap_or_default())
}

/// Whether an open agent still runs as this conversation, which the daemon
/// finds by the conversation's identity when it retires the agent.
fn bound_to_open_agent(
    session: &crate::chat::Session,
    agents: &[crate::agent_execution::Agent],
) -> bool {
    agents.iter().any(|agent| {
        !agent.closed
            && matches!(
                &agent.session,
                Some(crate::agent_execution::HostedSession::Chat(bound)) if bound.id == session.id
            )
    })
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
    SessionsClosed(ChannelId, uuid::Uuid, Result<(), String>),
    RetryDelete(ChannelId),
    /// Runs a deletion that failed again, from the channel's row.
    RetryFailed(ChannelId),
    ReopenTimedOut(ChannelId, std::time::Instant),
    /// Retires the emptied channels whose views have already settled, once
    /// the closes that emptied them have landed. A pane that never reached
    /// its shared view, such as an unsent conversation, publishes no view
    /// edit whose answer would otherwise do this.
    SettleRetirements,
}

impl Action {
    /// Whether this action is a dialog answer or an asynchronous completion.
    ///
    /// Everything else a channel can be asked to do is a navigation command
    /// and stands down behind any modal; refusing the dialog's own answers
    /// would strand it with no way out.
    pub fn answers_own_dialog(&self) -> bool {
        matches!(
            self,
            Self::ConfirmDelete
                | Self::Dismiss
                | Self::SessionsClosed(..)
                | Self::RetryDelete(_)
                | Self::ReopenTimedOut(..)
                | Self::SettleRetirements
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
    /// Exact local targets retained until the matching close worker completes.
    closing: HashMap<ChannelId, ChannelCloseAttempt>,
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
    /// Open panes discarded with a deleted channel have no normal close
    /// history, and the registry also removes their channel view. Keep their
    /// known destination so the saved picker cannot file them into whichever
    /// channel happens to be selected later. Pane identities are not reused.
    deleted_cards: HashSet<CardId>,
    /// Channels this window closed the last card of, as they were then, so a
    /// card reopened from one can bring it back after it retires. Explicit
    /// deletion forgets the channel; only automatic retirement is undone.
    emptied: Vec<EmptiedChannel>,
    /// The channel each retired one was revived as. A retired identity is
    /// never reused, so a revival is a new channel, and every other card
    /// closed in the old one follows it there.
    revived: HashMap<ChannelId, ChannelId>,
    /// Automatic names to give back to channels a closed card returned to.
    /// The registry forgets an automatic name when a channel's conversation
    /// leaves, and a conversation that comes back as a new card does not
    /// reclaim it until it reports a name of its own, which one that fails
    /// to reconnect never does.
    names: HashMap<ChannelId, (String, crate::model::ChannelTitleSource)>,
    reopening: HashMap<ChannelId, PendingReopen>,
    /// Emptied channels waiting for their view publication to settle, by the
    /// incarnation the close saw. The registry checks that incarnation and
    /// the settled view revision before it retires one.
    pub(super) retire_candidates: HashSet<crate::channel_work::Home>,
    /// Retirements sent to the registry, with the view revision each expects
    /// and when it was sent. The registry applies edits in order, so a pane
    /// published into one of these channels before the answer arrives would
    /// land after it retired.
    pub(super) retiring: HashMap<ChannelId, (u64, std::time::Instant)>,
    /// Where the one sweep for channels emptied while nobody watched stands.
    pub(super) sweep: Sweep,
}

/// The startup sweep waits for this window's restore to verify what it saved,
/// then runs once the registry is joined.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) enum Sweep {
    #[default]
    Waiting,
    Ready,
    Done,
}

/// Bound a deferred reopen's wait without assuming the retirement was refused.
const RETIREMENT_ANSWERED: std::time::Duration = std::time::Duration::from_secs(10);

#[derive(Debug)]
struct PendingReopen {
    home: crate::channel_work::Home,
    sent: std::time::Instant,
    intents: Vec<Reopen>,
}

#[derive(Debug)]
pub(super) enum Reopen {
    Conversation {
        entry: crate::persistence::SavedConversation,
        prepared: bool,
        generation: u64,
        replace: Option<CardId>,
    },
    Pane(CardId),
}

impl Reopen {
    fn card(&self) -> CardId {
        match self {
            Self::Conversation { entry, .. } => entry.card,
            Self::Pane(card) => *card,
        }
    }
}

/// What a channel was when its last card closed: enough to recreate it as the
/// user knew it, under its own name and in its own project.
#[derive(Debug, Clone)]
struct EmptiedChannel {
    id: ChannelId,
    project_id: ProjectId,
    title: String,
    title_source: crate::model::ChannelTitleSource,
}

/// As many emptied channels as there are recently closed cards to reopen.
const EMPTIED_CHANNELS: usize = 32;

/// Where a closed card was when it closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ClosedCardHome {
    /// A channel that still exists and can take the card back.
    Channel(ChannelId),
    /// The registry must decide whether the original identity survives before
    /// another pane can be published into it.
    Pending(ChannelId),
    /// A channel that retired when its last card closed, which reopening the
    /// card recreates.
    Retired(ChannelId),
    /// A worktree or folder strip, or a channel that is gone for good.
    Strip,
}

#[derive(Debug)]
struct ChannelCloseAttempt {
    operation: uuid::Uuid,
    registry: uuid::Uuid,
    project: ProjectId,
    members: Vec<CardId>,
    inactive: Vec<CardId>,
    panes: Vec<(CardId, CardKind, bool)>,
}

// Closing a terminal may publish its ended projection before the worker's
// completion. Allow that projection, but never a different session, launch,
// proof, recovery generation, or native conversation descriptor.
fn closed_channel_source_matches(before: &CardKind, current: &CardKind) -> bool {
    if before == current {
        return true;
    }
    let (CardKind::Terminal(before), CardKind::Terminal(current)) = (before, current) else {
        return false;
    };
    if before.continuity.live_proof().is_none() || !terminal_ended(current) {
        return false;
    }
    if let TerminalContinuity::RelaunchAvailable { recipe, .. } = &current.continuity
        && recipe != &before.launch
    {
        return false;
    }
    let mut ended = current.clone();
    ended.continuity = before.continuity.clone();
    &ended == before
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

    #[cfg(test)]
    pub(super) fn close_operation(&self, id: ChannelId) -> uuid::Uuid {
        self.closing
            .get(&id)
            .expect("channel close worker")
            .operation
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
        widget::Id::new("dialog.channel.close")
    }
}

impl Cagents {
    /// Utility panes and closed chats do not own the channel conversation name.
    pub(super) fn channel_primary_chat(&self, channel: &ProjectChannel) -> Option<CardId> {
        channel.cards.iter().copied().find(|id| {
            self.card_state(*id).is_some_and(|card| {
                card.existence.is_open() && matches!(card.kind, CardKind::Chat(_))
            })
        })
    }

    /// Opens channel `id` as navigation alone; a caller that also changed the
    /// workspace saves it.
    fn open_channel_with_default(&mut self, id: ChannelId) -> Task<Message> {
        let scope = self.navigate_to_channel(id);
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
        if let Action::SettleRetirements = action {
            self.retire_settled_empty_channels();
            return Task::none();
        }
        if let Action::ReopenTimedOut(id, sent) = action {
            if self
                .channel_ui
                .reopening
                .get(&id)
                .is_some_and(|pending| pending.sent == sent)
            {
                self.fail_channel_reopen(id, "Could not confirm whether the channel closed. Reconnect the service and try reopening again.");
            }
            return Task::none();
        }
        if let Action::RetryDelete(id) = action {
            if self.channel_ui.closing.contains_key(&id) {
                return Task::none();
            }
            self.channel_ui.deleting.remove(&id);
            return self.delete_channel(id);
        }
        if let Action::SessionsClosed(id, operation, result) = action {
            if !self
                .channel_ui
                .closing
                .get(&id)
                .is_some_and(|attempt| attempt.operation == operation)
            {
                return Task::none();
            }
            let attempt = self.channel_ui.closing.remove(&id).unwrap();
            self.channel_ui.deleting.remove(&id);
            let result = result.and_then(|()| {
                self.channel_close_targets_current(id, &attempt).then_some(()).ok_or_else(|| {
                    "The channel's panes changed while sessions were closing; review them before deleting again".into()
                })
            });
            return match result {
                Ok(()) => {
                    // Managed teardown can remove these captured-open panes
                    // from the channel before its validated receipt arrives.
                    // Remember their deleted origin while the exact capture
                    // still names them; they are not ordinary undo history.
                    self.channel_ui.deleted_cards.extend(
                        attempt
                            .panes
                            .iter()
                            .filter(|(_, kind, _)| !kind.is_utility())
                            .map(|(card, _, _)| *card),
                    );
                    self.finish_delete_channel(id)
                }
                Err(error) => {
                    let title = self
                        .durable
                        .channels
                        .iter()
                        .find(|channel| channel.id == id)
                        .map(|channel| {
                            channel_display_title(
                                channel,
                                self.sidebar_model.project(channel.project_id),
                            )
                        })
                        .unwrap_or(Cow::Borrowed("the channel"));
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
            Action::SessionsClosed(..)
            | Action::RetryDelete(_)
            | Action::ReopenTimedOut(..)
            | Action::SettleRetirements => {
                unreachable!()
            }
            Action::RetryFailed(id) => {
                if !self.channel_ui.failed.contains_key(&id) {
                    return Task::none();
                }
                self.delete_channel(id)
            }
            Action::Create(project_id) => {
                let Some(id) = self.create_channel(project_id, String::new(), Default::default())
                else {
                    return Task::none();
                };
                let scope = self.open_channel_with_default(id);
                self.persist();
                Task::batch([
                    scope,
                    sidebar::reveal(sidebar::selection_row_focus_id(
                        sidebar::SelectionRow::Channel(id),
                    ))
                    .map(cosmic::Action::App),
                ])
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

    /// Adds a channel to `project_id` and publishes it, without entering it.
    ///
    /// An empty `title` means the channel awaits its first accepted prompt; a
    /// nonempty one, including a user's, is never replaced by that prompt.
    fn create_channel(
        &mut self,
        project_id: ProjectId,
        title: String,
        title_source: crate::model::ChannelTitleSource,
    ) -> Option<ChannelId> {
        let project = self
            .sidebar_model
            .projects
            .iter()
            .find(|project| project.id == project_id)?;
        if self.project_ui.removing.contains_key(&project.repository)
            || self.project_ui.closed.contains(&project.repository)
        {
            self.status = Some(TransientNotice::warning("That project is being removed."));
            return None;
        }
        let directory = self.project_primary_worktree(project_id)?;
        if let Err(error) = self.worktree_protections.protect_next(&directory) {
            self.status = Some(TransientNotice::warning(format!(
                "Cannot open this worktree yet: {error}"
            )));
            return None;
        }
        let id = ChannelId::new();
        // The registry assigns the real rank and this window learns it from
        // the answer to the publish below. Until then it takes one more than
        // the highest rank it knows, which is what the registry would assign
        // and what keeps two channels created before any answer arrives in the
        // order they were made.
        let ordinal = self.next_provisional_ordinal();
        Arc::make_mut(&mut self.durable)
            .channels
            .push(ProjectChannel {
                title_source,
                id,
                project_id,
                title,
                ordinal,
                // Not shared until the registry answers the publish below.
                // Claiming it early would let a registry that never received
                // the create retire it on a reconnect.
                shared: false,
                cards: Vec::new(),
                focused: None,
            });
        self.publish_channel_creation(id, project_id);
        self.sidebar_state.reveal_project(project_id);
        Some(id)
    }

    /// Where `card` was when it closed, when this window can tell: the scope
    /// it was closed from, or else the one shared view that records removing
    /// it. A deleted origin or more than one such view identifies no surviving
    /// channel, so the card returns to its worktree. Only an unknown origin
    /// leaves placement to the channel where the picker was opened.
    pub(super) fn closed_card_home(&self, card: CardId) -> Option<ClosedCardHome> {
        if self.channel_ui.deleted_cards.contains(&card) {
            return Some(ClosedCardHome::Strip);
        }
        let closed_from = self
            .recently_closed
            .iter()
            .rev()
            .find(|(state, _)| state.id == card)
            .and_then(|(_, destination)| destination.as_ref());
        let channel = match closed_from {
            Some(WorkspaceDestination::Channel(id)) => *id,
            Some(WorkspaceDestination::Worktree(_) | WorkspaceDestination::Folder(_)) => {
                return Some(ClosedCardHome::Strip);
            }
            None => {
                let mut views = self
                    .workspace_model
                    .views
                    .iter()
                    .filter(|view| view.retired.contains(&card));
                let view = views.next()?;
                if views.next().is_some() {
                    return Some(ClosedCardHome::Strip);
                }
                match view.scope {
                    crate::workspace_model::views::Scope::Channel(id) => id,
                    crate::workspace_model::views::Scope::Worktree { .. } => {
                        return Some(ClosedCardHome::Strip);
                    }
                }
            }
        };
        self.closed_channel_home(channel)
    }

    /// The channel a card closed in `id` returns to: `id` itself, or the
    /// channel that revived it, while it is still there to enter, or else a
    /// retired one this window can recreate.
    pub(super) fn closed_channel_home(&self, mut id: ChannelId) -> Option<ClosedCardHome> {
        // Each revival maps a retired identity to a newer one, so the chain
        // is acyclic and no longer than the map.
        for _ in 0..self.channel_ui.revived.len() {
            match self.channel_ui.revived.get(&id) {
                Some(&next) => id = next,
                None => break,
            }
        }
        let present = self.durable.channels.iter().any(|channel| channel.id == id)
            && !self.channel_ui.is_deleting(id);
        if present {
            return Some(if self.channel_retiring(id) {
                ClosedCardHome::Pending(id)
            } else {
                ClosedCardHome::Channel(id)
            });
        }
        // A channel gone any other way, deleted by the user in any window,
        // is gone for good, and its cards reopen in their worktree.
        Some(
            if self
                .channel_ui
                .emptied
                .iter()
                .any(|emptied| emptied.id == id)
            {
                ClosedCardHome::Retired(id)
            } else {
                ClosedCardHome::Strip
            },
        )
    }

    /// Whether a card closed in channel `id` reopens from `scope`: from the
    /// channel it now resolves to, or, once that channel retired, from the
    /// project's worktrees, where the window fell back to when it retired.
    /// Another channel keeps reopening its own panes.
    pub(super) fn closed_channel_reopens_from(
        &self,
        id: ChannelId,
        scope: Option<WorkspaceDestination>,
    ) -> bool {
        match self.closed_channel_home(id) {
            Some(ClosedCardHome::Channel(home) | ClosedCardHome::Pending(home)) => {
                scope == Some(WorkspaceDestination::Channel(home))
            }
            Some(ClosedCardHome::Retired(retired)) => {
                let project = self
                    .channel_ui
                    .emptied
                    .iter()
                    .find(|emptied| emptied.id == retired)
                    .map(|emptied| emptied.project_id);
                !matches!(scope, Some(WorkspaceDestination::Channel(_)))
                    && project.is_some()
                    && project == self.destination_project(scope)
            }
            Some(ClosedCardHome::Strip) | None => false,
        }
    }

    /// A timed out answer does not cancel the edit already in the publisher.
    /// Keep its identity fenced until the registry answers definitively.
    fn channel_retiring(&self, id: ChannelId) -> bool {
        self.channel_ui.retiring.contains_key(&id)
    }

    pub(super) fn defer_channel_reopen(
        &mut self,
        home: ClosedCardHome,
        intent: Reopen,
    ) -> Option<Task<Message>> {
        let ClosedCardHome::Pending(id) = home else {
            return None;
        };
        let creation_ordinal = self
            .workspace_model
            .channel(id)
            .map(|channel| channel.ordinal)
            .or_else(|| {
                self.durable
                    .channels
                    .iter()
                    .find(|channel| channel.id == id)
                    .map(|channel| channel.ordinal)
            })?;
        let home = crate::channel_work::Home {
            registry: self.workspace_model.identity,
            channel: id,
            creation_ordinal,
        };
        let pending = self
            .channel_ui
            .reopening
            .entry(id)
            .or_insert_with(|| PendingReopen {
                home,
                sent: std::time::Instant::now(),
                intents: Vec::new(),
            });
        if let Some(index) = pending
            .intents
            .iter()
            .position(|current| current.card() == intent.card())
        {
            // A cancelled picker can select the same conversation in a new
            // generation. Keep the latest intent, including its cancel fence.
            pending.intents[index] = intent;
            return Some(Task::none());
        }
        pending.intents.push(intent);
        let sent = pending.sent;
        Some(cosmic::task::future(async move {
            tokio::time::sleep(RETIREMENT_ANSWERED).await;
            Message::ChannelUi(Action::ReopenTimedOut(id, sent))
        }))
    }

    pub(super) fn finish_channel_reopen(&mut self, id: ChannelId) -> Task<Message> {
        let Some(pending) = self.channel_ui.reopening.remove(&id) else {
            return Task::none();
        };
        let same_home = pending.home.registry == self.workspace_model.identity
            && self
                .workspace_model
                .channel(id)
                .is_none_or(|channel| channel.ordinal == pending.home.creation_ordinal);
        let mut tasks = Vec::new();
        for intent in pending.intents {
            match intent {
                Reopen::Conversation {
                    entry,
                    prepared,
                    generation,
                    replace,
                } => {
                    tasks.push(self.finish_deferred_conversation(
                        entry, prepared, generation, replace, same_home,
                    ));
                }
                Reopen::Pane(card) if same_home => tasks.push(self.reopen_closed_pane(card)),
                Reopen::Pane(_) => {}
            }
        }
        Task::batch(tasks)
    }

    pub(super) fn fail_channel_reopen(&mut self, id: ChannelId, error: &str) {
        let Some(pending) = self.channel_ui.reopening.remove(&id) else {
            return;
        };
        let mut active = false;
        for intent in pending.intents {
            match intent {
                Reopen::Conversation { generation, .. } => {
                    active |= self.fail_deferred_conversation(generation, error);
                }
                Reopen::Pane(_) => active = true,
            }
        }
        if active {
            self.status = Some(TransientNotice::warning(error));
        }
    }

    pub(super) fn forget_channel_revival(&mut self, id: ChannelId) {
        self.channel_ui.emptied.retain(|emptied| emptied.id != id);
        self.channel_ui.revived.remove(&id);
        self.channel_ui.retiring.remove(&id);
    }

    /// Enters the channel a closed card returns to, recreating it if it
    /// retired, and keeps it from retiring under the card about to arrive.
    /// `None` when the card belongs to no channel this window can enter.
    pub(super) fn enter_closed_card_home(&mut self, home: ClosedCardHome) -> Option<Task<Message>> {
        let (id, reveal) = match home {
            ClosedCardHome::Channel(id) => {
                self.channel_ui
                    .retire_candidates
                    .retain(|candidate| candidate.channel != id);
                // No longer empty, so no longer a channel to revive, but one
                // whose automatic name left with its conversation.
                if let Some(index) = self
                    .channel_ui
                    .emptied
                    .iter()
                    .position(|emptied| emptied.id == id)
                {
                    let emptied = self.channel_ui.emptied.remove(index);
                    self.remember_channel_name(id, emptied.title, emptied.title_source);
                }
                (id, Task::none())
            }
            ClosedCardHome::Retired(id) => {
                let id = self.revive_channel(id)?;
                (
                    id,
                    sidebar::reveal(sidebar::selection_row_focus_id(
                        sidebar::SelectionRow::Channel(id),
                    ))
                    .map(cosmic::Action::App),
                )
            }
            ClosedCardHome::Pending(_) | ClosedCardHome::Strip => return None,
        };
        let scope = self.select_channel(id);
        (self.durable.active_channel == Some(id)).then(|| Task::batch([scope, reveal]))
    }

    /// The project `scope` belongs to.
    fn destination_project(&self, scope: Option<WorkspaceDestination>) -> Option<ProjectId> {
        match scope? {
            WorkspaceDestination::Channel(id) => self
                .durable
                .channels
                .iter()
                .find(|channel| channel.id == id)
                .map(|channel| channel.project_id),
            WorkspaceDestination::Folder(project) => Some(project),
            WorkspaceDestination::Worktree(worktree) => self
                .sidebar_model
                .projects
                .iter()
                .find(|project| project.worktrees.iter().any(|row| row.id == worktree))
                .map(|project| project.id),
        }
    }

    /// Takes back a revival that received no card, so the channel is not left
    /// empty where nothing would retire it, and stays revivable.
    pub(super) fn abandon_empty_revival(
        &mut self,
        retired: ChannelId,
        previous: (Option<ChannelId>, Option<PathBuf>),
    ) -> Task<Message> {
        let Some(&revived) = self.channel_ui.revived.get(&retired) else {
            return Task::none();
        };
        if self.channel_has_open_card(revived) {
            return Task::none();
        }
        let Some(channel) = self
            .durable
            .channels
            .iter()
            .find(|channel| channel.id == revived)
        else {
            return Task::none();
        };
        let snapshot = EmptiedChannel {
            id: retired,
            project_id: channel.project_id,
            title: channel.title.clone(),
            title_source: channel.title_source,
        };
        self.channel_ui.revived.remove(&retired);
        self.remember_emptied(snapshot);
        let retired = self.retire_channel(revived, false);
        // Back where reopening started, not wherever retiring would land.
        let returned = match previous {
            (Some(channel), _) if self.durable.channels.iter().any(|row| row.id == channel) => {
                self.select_channel(channel)
            }
            (_, Some(path)) => self.select_worktree(path, false),
            _ => Task::none(),
        };
        Task::batch([retired, returned])
    }

    /// Keeps a snapshot of a channel this window emptied, newest last.
    fn remember_emptied(&mut self, snapshot: EmptiedChannel) {
        let state = &mut self.channel_ui;
        state.emptied.retain(|emptied| emptied.id != snapshot.id);
        state.emptied.push(snapshot);
        if state.emptied.len() > EMPTIED_CHANNELS {
            state.emptied.remove(0);
        }
    }

    /// Recreates a channel that retired when its last card closed, under its
    /// name and in its project. The old identity is gone for good, so the
    /// channel returns as a new one that the old identity now resolves to.
    fn revive_channel(&mut self, id: ChannelId) -> Option<ChannelId> {
        let index = self
            .channel_ui
            .emptied
            .iter()
            .position(|emptied| emptied.id == id)?;
        let emptied = self.channel_ui.emptied[index].clone();
        let revived = self.create_channel(
            emptied.project_id,
            emptied.title.clone(),
            emptied.title_source,
        )?;
        self.channel_ui.emptied.remove(index);
        self.channel_ui.revived.insert(id, revived);
        self.remember_channel_name(revived, emptied.title, emptied.title_source);
        Some(revived)
    }

    /// A manual name survives its conversation leaving, so only an automatic
    /// one needs giving back.
    fn remember_channel_name(
        &mut self,
        id: ChannelId,
        title: String,
        source: crate::model::ChannelTitleSource,
    ) {
        if source != crate::model::ChannelTitleSource::Manual && !title.is_empty() {
            self.channel_ui.names.insert(id, (title, source));
        }
    }

    /// Gives a channel back the automatic name it had once the registry holds
    /// the pane that returned to it, which is when it forgets the name. Any
    /// name it holds by then, the conversation's own or one typed, wins.
    ///
    /// The name is the registry's own from before, not a new inference, so it
    /// needs no live conversation to vouch for it, which one that failed to
    /// reconnect could not.
    pub(super) fn restore_channel_names(&mut self) {
        let names = std::mem::take(&mut self.channel_ui.names);
        for (id, (title, source)) in names {
            let Some(index) = self
                .durable
                .channels
                .iter()
                .position(|channel| channel.id == id)
            else {
                continue;
            };
            let arrived = self.workspace_model.views.iter().any(|view| {
                view.scope == crate::workspace_model::views::Scope::Channel(id)
                    && view
                        .panes
                        .iter()
                        .any(|pane| !view.retired.contains(&pane.id))
            });
            let Some(named) = self
                .workspace_model
                .channel(id)
                .map(|shared| !shared.title.is_empty())
                .filter(|_| arrived)
            else {
                self.channel_ui.names.insert(id, (title, source));
                continue;
            };
            if named {
                continue;
            }
            let channel = &mut Arc::make_mut(&mut self.durable).channels[index];
            channel.title = title.clone();
            channel.title_source = source;
            self.publish_model_edit(ModelEdit::SuggestChannelTitle { id, title, source });
            self.refresh_sidebar_channels();
            self.persist();
        }
    }

    /// Defer automatic retirement until the canonical view publication settles.
    /// A work-bearing channel survives its final conversation, across windows.
    pub(super) fn close_emptied_channels(&mut self, retired: &[CardId]) -> Task<Message> {
        let emptied = self.channels_emptied_by(retired);
        if emptied.is_empty() {
            return Task::none();
        }
        self.capture_retire_candidates(&emptied);
        // The closing card is still open here; settle after its close lands.
        // A published pane is still in its view until the registry answers
        // the removal, which keeps the channel waiting for that answer.
        cosmic::task::message(Message::ChannelUi(Action::SettleRetirements))
    }

    /// Retires channel `left`, which this window just navigated away from,
    /// when it holds nothing, as closing its last card would have. A channel
    /// can reach that state without one of its cards closing here: a pane
    /// moved out, a discarded composer, a peer's close, or a restore.
    pub(super) fn retire_left_channel(&mut self, left: Option<ChannelId>) {
        let Some(left) = left.filter(|&id| self.durable.active_channel != Some(id)) else {
            return;
        };
        if self.channel_holds_nothing(left) {
            self.capture_retire_candidates(&[left]);
            self.retire_settled_empty_channels();
        }
    }

    /// Asks once, after this window's restore settled and it joined the
    /// registry, to retire every channel it is not showing that holds
    /// nothing. Startup, including the relaunch an update makes, is when
    /// channels emptied while no window watched them are found.
    pub(super) fn sweep_empty_channels(&mut self) {
        let sweep = &mut self.channel_ui.sweep;
        if *sweep != Sweep::Ready
            || self.shared_views.registry_unavailable
            || self.workspace_model.identity == crate::workspace_model::NO_REGISTRY
        {
            return;
        }
        *sweep = Sweep::Done;
        let empty = self
            .durable
            .channels
            .iter()
            .map(|channel| channel.id)
            .filter(|&id| self.durable.active_channel != Some(id) && self.channel_holds_nothing(id))
            .collect::<Vec<_>>();
        if empty.is_empty() {
            return;
        }
        tracing::debug!(
            count = empty.len(),
            "retiring channels found empty at startup"
        );
        self.capture_retire_candidates(&empty);
        self.retire_settled_empty_channels();
    }

    /// The restore this window was launched to perform verified everything it
    /// saved, so the sweep may run once the registry is also joined. A failed
    /// restore never sweeps: what it left unattached must stay visible.
    pub(super) fn allow_empty_channel_sweep(&mut self) {
        if self.channel_ui.sweep == Sweep::Waiting {
            self.channel_ui.sweep = Sweep::Ready;
        }
        self.sweep_empty_channels();
    }

    /// Whether `id` holds nothing a person could lose with it, as this window
    /// and its copy of the registry see it: no open card of its own here, no
    /// pane in its shared view, and no session waiting to be shown in it.
    /// The registry repeats the view, work and session checks when it is
    /// asked to retire the channel.
    fn channel_holds_nothing(&self, id: ChannelId) -> bool {
        let Some(channel) = self
            .durable
            .channels
            .iter()
            .find(|channel| channel.id == id)
        else {
            return false;
        };
        if self.workspace_model.channel(id).is_none()
            || self.channel_ui.is_deleting(id)
            || self.channel_retiring(id)
            || self.channel_ui.reopening.contains_key(&id)
        {
            return false;
        }
        let open_content = channel.cards.iter().any(|&card| {
            self.is_channel_content(card)
                && self
                    .card_state(card)
                    .is_some_and(|state| state.existence.is_open())
        });
        if open_content {
            return false;
        }
        self.workspace_model
            .views
            .iter()
            .find(|view| view.scope == crate::workspace_model::views::Scope::Channel(id))
            .is_some_and(|view| view.panes.is_empty() && !self.channel_awaits_session(id, view))
    }

    /// Whether an open agent placed in `id` has not been shown in it yet, or
    /// failed to be: a detached session, which keeps the channel.
    fn channel_awaits_session(
        &self,
        id: ChannelId,
        view: &crate::workspace_model::views::View,
    ) -> bool {
        self.workspace_model
            .agents
            .iter()
            .any(|agent| agent.awaits_pane_in(id, &view.retired))
    }

    /// Records `ids` as retirement candidates under the incarnation the
    /// registry now holds, each with a snapshot that lets a card reopened from
    /// it bring it back.
    fn capture_retire_candidates(&mut self, ids: &[ChannelId]) {
        for &id in ids {
            let Some(channel) = self
                .durable
                .channels
                .iter()
                .find(|channel| channel.id == id)
            else {
                continue;
            };
            let snapshot = EmptiedChannel {
                id,
                project_id: channel.project_id,
                title: channel.title.clone(),
                title_source: channel.title_source,
            };
            self.remember_emptied(snapshot);
        }
        let captured = ids.iter().filter_map(|&id| {
            let channel = self.workspace_model.channel(id)?;
            let home = crate::channel_work::Home {
                registry: self.workspace_model.identity,
                channel: id,
                creation_ordinal: channel.ordinal,
            };
            home.validate().ok().map(|_| home)
        });
        self.channel_ui.retire_candidates.extend(captured);
    }

    /// View publication supplies the settled revision. The registry checks
    /// that exact revision and the channel's incarnation before retiring it.
    /// Until the daemon's channel work store is retired, it also keeps a
    /// channel whose incarnation has channel work entries or operations.
    pub(super) fn retire_settled_empty_channels(&mut self) {
        let candidates = self
            .channel_ui
            .retire_candidates
            .iter()
            .copied()
            .collect::<Vec<_>>();
        for captured in candidates {
            let channel = captured.channel;
            let Some(metadata) = self.workspace_model.channel(channel) else {
                self.channel_ui.retire_candidates.remove(&captured);
                continue;
            };
            if self.workspace_model.identity != captured.registry
                || metadata.ordinal != captured.creation_ordinal
            {
                self.channel_ui.retire_candidates.remove(&captured);
                continue;
            }
            let Some(view) =
                self.workspace_model.views.iter().find(|view| {
                    view.scope == crate::workspace_model::views::Scope::Channel(channel)
                })
            else {
                continue;
            };
            if !view.panes.is_empty() {
                continue;
            }
            // An empty view can precede a live pane's first publication. Closing
            // that pane later captures the channel again.
            // So can an agent's first adoption, which a failed restore can
            // leave undone; that session is not emptiness either.
            if self.channel_holds_unpublished_work(channel)
                || self.channel_awaits_session(channel, view)
            {
                self.channel_ui.retire_candidates.remove(&captured);
                continue;
            }
            let edit = ModelEdit::RetireEmptyChannel {
                home: captured,
                expected_view_revision: view.revision,
            };
            if self.workspace_model_tx.try_send(edit).is_ok() {
                self.channel_ui.retire_candidates.remove(&captured);
                self.channel_ui
                    .retiring
                    .insert(channel, (view.revision, std::time::Instant::now()));
            }
        }
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

    /// An automatic composer nobody used does not keep a channel alive; any
    /// other open content does, whether or not the registry has heard of it.
    pub(super) fn channel_holds_unpublished_work(&self, id: ChannelId) -> bool {
        self.durable
            .channels
            .iter()
            .find(|channel| channel.id == id)
            .is_some_and(|channel| {
                channel.cards.iter().any(|&card| {
                    self.is_channel_content(card)
                        && !self.chat_is_unused_default(card)
                        && !self.shared_views.defaults.contains_key(&card)
                        && self
                            .card_state(card)
                            .is_some_and(|state| state.existence.is_open())
                })
            })
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
        let title = channel_display_title(channel, self.sidebar_model.project(channel.project_id))
            .into_owned();
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
        if cards.iter().any(|card| self.pane_transfer_pending(*card)) {
            self.status = Some(TransientNotice::warning(
                "Wait for the pane move to finish before deleting this channel.",
            ));
            return Task::none();
        }
        if !self.channel_ui.deleting.insert(id) {
            return Task::none();
        }
        // A channel the user deleted stays deleted; reopening one of its
        // cards does not bring it back.
        self.fail_channel_reopen(
            id,
            "Reopening was cancelled because the channel is being deleted.",
        );
        self.forget_channel_revival(id);
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
        let operation = uuid::Uuid::new_v4();
        let panes = self
            .durable
            .cards
            .iter()
            .filter(|card| cards.contains(&card.id) && card.existence.is_open())
            .map(|card| {
                (
                    card.id,
                    card.kind.clone(),
                    self.workspace_model
                        .views
                        .iter()
                        .any(|view| view.panes.iter().any(|pane| pane.id == card.id)),
                )
            })
            .collect();
        let project = self
            .durable
            .channels
            .iter()
            .find(|channel| channel.id == id)
            .unwrap()
            .project_id;
        let inactive = cards
            .iter()
            .copied()
            .filter(|card| {
                self.card_state(*card)
                    .is_none_or(|state| !state.existence.is_open())
            })
            .collect();
        self.channel_ui.closing.insert(
            id,
            ChannelCloseAttempt {
                operation,
                registry: self.workspace_model.identity,
                project,
                members: cards,
                inactive,
                panes,
            },
        );
        let close = cosmic::task::future(async move {
            let result =
                tokio::task::spawn_blocking(move || close_sessions_retiring_agents(sessions))
                    .await
                    .map_err(|error| error.to_string())
                    .and_then(|result| result);
            Message::ChannelUi(Action::SessionsClosed(id, operation, result))
        });
        Task::batch([leave, close])
    }

    fn channel_close_targets_current(&self, id: ChannelId, attempt: &ChannelCloseAttempt) -> bool {
        let Some(channel) = self
            .durable
            .channels
            .iter()
            .find(|channel| channel.id == id)
        else {
            return false;
        };
        if self.workspace_model.identity != attempt.registry
            || channel.project_id != attempt.project
            || attempt.inactive.iter().any(|card| {
                self.card_state(*card)
                    .is_some_and(|state| state.existence.is_open())
            })
        {
            return false;
        }
        // A close can publish its managed pane tombstone before the worker's
        // receipt arrives. Only that exact source removal may shrink the
        // captured membership; additions, moves and replacements still fail.
        let removed = |card: CardId| {
            attempt.panes.iter().any(|(captured, before, was_shared)| {
                *captured == card
                    && *was_shared
                    && self.workspace_model.views.iter().any(|view| {
                        view.scope == crate::workspace_model::views::Scope::Channel(id)
                            && view.retired.contains(&card)
                    })
                    && !self
                        .workspace_model
                        .views
                        .iter()
                        .any(|view| view.panes.iter().any(|pane| pane.id == card))
                    && !self.pane_transfer_pending(card)
                    && !self
                        .durable
                        .channels
                        .iter()
                        .any(|channel| channel.id != id && channel.cards.contains(&card))
                    && self
                        .card_state(card)
                        .is_some_and(|current| closed_channel_source_matches(before, &current.kind))
            })
        };
        let mut current = channel.cards.iter().peekable();
        for original in &attempt.members {
            if current.peek().is_some_and(|next| *next == original) {
                current.next();
            } else if !removed(*original) {
                return false;
            }
        }
        if current.next().is_some() {
            return false;
        }
        attempt.panes.iter().all(|(card, before, was_shared)| {
            let mut owners = self.workspace_model.views.iter().flat_map(|view| {
                view.panes
                    .iter()
                    .filter(move |pane| pane.id == *card)
                    .map(move |pane| (&view.scope, pane))
            });
            let shared_current = match (owners.next(), owners.next()) {
                (None, None) => !was_shared || removed(*card),
                (Some((scope, pane)), None) => {
                    *scope == crate::workspace_model::views::Scope::Channel(id)
                        && closed_channel_source_matches(before, &pane.kind)
                }
                _ => false,
            };
            !self.pane_transfer_pending(*card)
                && !self
                    .durable
                    .channels
                    .iter()
                    .any(|channel| channel.id != id && channel.cards.contains(card))
                && shared_current
                && self
                    .card_state(*card)
                    .is_some_and(|current| closed_channel_source_matches(before, &current.kind))
        })
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
        // Only this window's own retirement of an emptied channel is undone by
        // reopening one of its cards; a deletion anywhere is final.
        let own_retirement = self.channel_ui.retiring.remove(&id).is_some();
        if !own_retirement {
            self.channel_ui.emptied.retain(|emptied| emptied.id != id);
        }
        let mut freed_utility = false;
        for card in channel.cards {
            // A utility pane outlives the channel it was visiting: it leaves
            // with no channel, which every worktree strip shows, and opening
            // it again from a channel brings it there.
            if !self.is_channel_content(card) {
                freed_utility = true;
                continue;
            }
            // Whoever deleted the channel could not have meant a conversation
            // that never reached its shared view.
            if passive
                && (self.shared_chat_has_local_work(card)
                    || self.chat_runs_unobserved_by_peers(card)
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
            if !own_retirement
                && self
                    .card_state(card)
                    .is_some_and(|state| state.existence.is_open())
            {
                self.channel_ui.deleted_cards.insert(card);
            }
            self.retire_lsp_card(card);
            self.sidebar_state.forget_card_attention(card);
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
                "A channel was deleted in another client. Your unsaved work and running conversations stay in its worktree.",
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
                    || {
                        project
                            .repository
                            .parent()
                            .filter(|parent| {
                                sidebar::is_primary_worktree(&project.repository, parent)
                            })
                            .unwrap_or(&project.repository)
                            .to_path_buf()
                    },
                    |worktree| worktree.path.clone(),
                ),
        )
    }

    pub(super) fn channel_dialog_view(&self) -> Option<Element<'_, Message>> {
        let dialog = self.channel_ui.dialog.as_ref()?;
        let mut rows: Vec<Element<'_, Message>> = vec![
            design::title("Delete channel?").into(),
            super::notice_body(match dialog.chats {
                0 => format!("Delete \"{}\" from this workspace?", dialog.title),
                1 => format!("Delete \"{}\" and close its conversation?", dialog.title),
                chats => format!(
                    "Delete \"{}\" and close its {chats} conversations?",
                    dialog.title
                ),
            }),
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
            .id(widget::Id::new("dialog.channel.confirm"))
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
                || channel.title_source != crate::model::ChannelTitleSource::Manual)
                && self.channel_primary_chat(channel) == Some(card)
        })
    }

    /// Names a new channel from its first submitted prompt or conversation name.
    ///
    /// Prompt fallbacks may later improve to provider titles. Manual and legacy
    /// titles remain authoritative, including renames from another window.
    pub(super) fn name_channel_from_prompt(&mut self, card: CardId, prompt: &str) {
        // Later messages do not replace an established task. Restored snapshots
        // may repair a poor historical fallback using the first substantive prompt.
        if self.durable.channels.iter().any(|channel| {
            channel.title.is_empty() && self.channel_primary_chat(channel) == Some(card)
        }) && let Some(title) = channel_title_from_prompt(prompt)
        {
            self.suggest_channel_title(card, title, crate::model::ChannelTitleSource::Prompt);
        }
    }

    pub(super) fn suggest_channel_title(
        &mut self,
        card: CardId,
        title: String,
        source: crate::model::ChannelTitleSource,
    ) -> bool {
        let Some(index) = self.durable.channels.iter().position(|channel| {
            channel.title_source.accepts_refresh(&channel.title, source)
                && self.channel_primary_chat(channel) == Some(card)
                && (channel.title != title || channel.title_source != source)
        }) else {
            return false;
        };
        let id = self.durable.channels[index].id;
        let Some(view) = self.workspace_model.views.iter().find(|view| {
            view.scope == crate::workspace_model::views::Scope::Channel(id)
                && view.primary_chat() == Some(card)
        }) else {
            return false;
        };
        let expected_revision = view.revision;
        Arc::make_mut(&mut self.durable).channels[index].title = title.clone();
        Arc::make_mut(&mut self.durable).channels[index].title_source = source;
        self.publish_model_edit(ModelEdit::SuggestConversationTitle {
            id,
            card,
            expected_revision,
            title,
            source,
        });
        self.refresh_sidebar_channels();
        self.persist();
        true
    }

    /// Refresh cached rows after state changes, never during rendering.
    pub(super) fn refresh_sidebar_channels(&mut self) {
        self.sidebar_state
            .set_channel_selected(self.durable.active_channel.is_some());
        self.sidebar_state
            .refresh_attention_projection(&self.durable, &self.sidebar_model.projects);
        let durable = &self.durable;
        let mut cards = HashMap::new();
        for card in durable
            .cards
            .iter()
            .filter(|card| card.existence.is_open() && !card.kind.is_utility())
        {
            cards.entry(card.id).or_insert(card);
        }
        let previews = &self.notification_ui.previews;
        let allow_previews = self.settings.notifications.message_previews;
        // One pass groups channels by project, keeping the durable vector's
        // order, so the refresh stays linear in channels and cards.
        let mut project_channels = HashMap::<_, Vec<_>>::new();
        for channel in &durable.channels {
            project_channels
                .entry(channel.project_id)
                .or_default()
                .push(channel);
        }
        // A failure outlives nothing: a channel that has gone since, however
        // it went, has nothing left to retry.
        if !self.channel_ui.failed.is_empty() {
            let live = durable
                .channels
                .iter()
                .map(|channel| channel.id)
                .collect::<HashSet<_>>();
            self.channel_ui.failed.retain(|id, _| live.contains(id));
        }
        let channel_ui = &self.channel_ui;
        let state = &self.sidebar_state;
        for project in &mut self.sidebar_model.projects {
            // One primary-branch lookup and placeholder per project refresh,
            // regardless of how many unnamed channels it contains.
            let unnamed = default_channel_title(Some(project));
            let mut index = 0;
            // Built in the durable vector's own order, which is the registry's
            // ascending creation ranks. Display order is not decided here:
            // `sidebar::channels_in_display_order` owns it, one named ordering
            // helper per row kind beside the one the worktree rows use.
            for channel in project_channels
                .get(&project.id)
                .into_iter()
                .flatten()
                .copied()
            {
                let title = if channel.title.is_empty() {
                    Cow::Borrowed(unnamed.as_ref())
                } else {
                    Cow::Borrowed(channel.title.as_str())
                };
                let selected = durable.active_channel == Some(channel.id);
                let deletion = channel_ui.row_deletion(channel.id);
                let mut conversations = BTreeMap::<_, sidebar::AttentionSummary>::new();
                let mut chats = 0;
                let mut has_panes = false;
                let mut notification_preview: Option<&super::notification_ui::Preview> = None;
                for id in &channel.cards {
                    // A visiting utility pane is not what the channel holds.
                    let Some(card) = cards.get(id) else {
                        continue;
                    };
                    has_panes = true;
                    if allow_previews && state.has_card_update(*id) {
                        let session = match &card.kind {
                            CardKind::Chat(session) => Some(session.id),
                            CardKind::Terminal(session) => Some(session.id),
                            _ => None,
                        };
                        if let Some(preview) = session.and_then(|session| previews.get(&session))
                            && notification_preview.is_none_or(|known| preview.rank < known.rank)
                        {
                            notification_preview = Some(preview);
                        }
                    }
                    if matches!(&card.kind, CardKind::Chat(_))
                        || matches!(&card.kind, CardKind::Terminal(session)
                            if crate::agent::Provider::recognize_launch(&session.launch).is_some())
                        || state.card_activity(*id).is_some()
                    {
                        chats += 1;
                    }
                    let attention = state.card_attention(*id);
                    if attention != sidebar::AttentionSummary::default() {
                        let resource = match &card.kind {
                            CardKind::Chat(session) => (0, session.id.as_uuid()),
                            CardKind::Terminal(session) => (0, session.id.as_uuid()),
                            _ => (1, card.id.as_uuid()),
                        };
                        conversations.entry(resource).or_default().union(attention);
                    }
                }
                let mut attention = sidebar::AttentionSummary::default();
                for conversation in conversations.into_values() {
                    attention.add(conversation);
                }
                let activity = attention.activity();
                let unread = attention.unread != 0;
                let notification_preview = notification_preview
                    .filter(|_| unread)
                    .map(|preview| &preview.text);
                let unchanged = project.channels.get(index).is_some_and(|row| {
                    row.id == channel.id
                        && row.title == title
                        && row.selected == selected
                        && row.activity == activity
                        && row.unread == unread
                        && row.attention == attention
                        && row.chats == chats
                        && row.has_panes == has_panes
                        && row.ordinal == channel.ordinal
                        && row.deletion == deletion
                        && row.notification_preview.as_ref() == notification_preview
                });
                if !unchanged {
                    let mut row = SidebarChannel::new(
                        channel.id,
                        title.into_owned(),
                        selected,
                        activity,
                        unread,
                        chats,
                        has_panes,
                        channel.ordinal,
                    )
                    .with_attention(attention);
                    row.deletion = deletion;
                    row = row.with_notification_preview(notification_preview.cloned());
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

/// A channel still waiting for its first accepted prompt has to be nameable
/// anyway: a row has to label it, and a dialog about to destroy it has to say
/// which one it means.
pub(super) fn channel_display_title<'a>(
    channel: &'a ProjectChannel,
    project: Option<&SidebarProject>,
) -> Cow<'a, str> {
    if !channel.title.is_empty() {
        return Cow::Borrowed(&channel.title);
    }
    default_channel_title(project)
}

fn default_channel_title(project: Option<&SidebarProject>) -> Cow<'static, str> {
    let branch = project.and_then(|project| {
        project
            .worktrees
            .iter()
            .find(|worktree| sidebar::is_primary_worktree(&project.repository, &worktree.path))
            .and_then(|worktree| worktree.branch.as_deref())
    });
    match branch {
        Some(branch) => Cow::Owned(format!(
            "New channel · {}",
            branch.strip_prefix("refs/heads/").unwrap_or(branch)
        )),
        None => Cow::Borrowed("New channel"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sidebar::RowDeletion;

    fn captured_home() -> crate::channel_work::Home {
        crate::channel_work::Home {
            registry: uuid::Uuid::new_v4(),
            channel: ChannelId::new(),
            creation_ordinal: 4,
        }
    }

    fn model_with_empty_channel(
        home: crate::channel_work::Home,
    ) -> crate::workspace_model::WorkspaceModel {
        use crate::workspace_model::views::{Scope, View};
        crate::workspace_model::WorkspaceModel {
            identity: home.registry,
            channels: vec![crate::workspace_model::ModelChannel {
                id: home.channel,
                repository: "/project".into(),
                title: "Work".into(),
                title_source: Default::default(),
                ordinal: home.creation_ordinal,
            }],
            views: vec![View {
                scope: Scope::Channel(home.channel),
                revision: 7,
                panes: vec![],
                retired: vec![],
            }],
            ..Default::default()
        }
    }

    #[test]
    fn a_channel_work_invalidation_never_skips_a_delayed_canonical_channel_edit() {
        use super::super::workspace_model_sync::ModelUpdate;
        let home = captured_home();
        let mut app = super::super::tests::context_inspector_test_app(false);
        app.workspace_model = model_with_empty_channel(home);
        app.workspace_model.sequence = 8;
        drop(app.apply_workspace_model_update(ModelUpdate::Edited {
            sequence: 10,
            edit: ModelEdit::ChannelWorkChanged { home },
        }));
        assert_eq!(
            app.workspace_model.sequence, 8,
            "an invalidation cannot acknowledge an unseen canonical edit"
        );
        drop(app.apply_workspace_model_update(ModelUpdate::Edited {
            sequence: 9,
            edit: ModelEdit::DeleteChannel { id: home.channel },
        }));
        assert!(app.workspace_model.channel(home.channel).is_none());
        assert_eq!(app.workspace_model.sequence, 9);
    }

    #[test]
    fn deferred_empty_retirement_never_rebinds_to_replacement_registry_or_channel_incarnation() {
        let original = captured_home();
        for current in [
            crate::channel_work::Home {
                registry: uuid::Uuid::new_v4(),
                ..original
            },
            crate::channel_work::Home {
                creation_ordinal: original.creation_ordinal + 1,
                ..original
            },
            original,
        ] {
            let mut app = super::super::tests::context_inspector_test_app(false);
            let (send, received) = async_channel::unbounded();
            app.workspace_model_tx = send;
            app.workspace_model = model_with_empty_channel(current);
            app.channel_ui.retire_candidates.insert(original);
            app.retire_settled_empty_channels();
            if current == original {
                assert_eq!(
                    received.try_recv().unwrap(),
                    ModelEdit::RetireEmptyChannel {
                        home: original,
                        expected_view_revision: 7
                    }
                );
            } else {
                assert!(
                    received.try_recv().is_err(),
                    "a delayed old close must not act on a replacement home"
                );
            }
            assert!(app.channel_ui.retire_candidates.is_empty());
        }
    }

    #[test]
    fn close_intent_captures_only_a_confirmed_original_home() {
        let original = captured_home();
        for ordinal in [
            original.creation_ordinal,
            crate::workspace_model::PENDING_ORDINAL,
        ] {
            let mut app = super::super::tests::context_inspector_test_app(true);
            let card = app.durable.cards[0].id;
            app.workspace_model = model_with_empty_channel(crate::channel_work::Home {
                creation_ordinal: ordinal,
                ..original
            });
            Arc::make_mut(&mut app.durable)
                .channels
                .push(crate::model::ProjectChannel {
                    id: original.channel,
                    project_id: ProjectId::new(),
                    title: "Work".into(),
                    title_source: Default::default(),
                    ordinal,
                    shared: true,
                    cards: vec![card],
                    focused: Some(card),
                });
            drop(app.close_emptied_channels(&[card]));
            assert_eq!(
                app.channel_ui.retire_candidates.contains(&original),
                ordinal != crate::workspace_model::PENDING_ORDINAL
            );
        }
    }

    #[test]
    fn deletion_opens_on_cancel_and_enter_or_escape_keeps_the_channel_and_cards() {
        for enter in [false, true] {
            let (mut app, channel, _survivor, card) = channel_with_a_conversation();
            let original = app.card_state(card).unwrap().clone();
            let opening = app.handle_channel_ui(Action::Delete(channel));
            assert_eq!(
                super::super::tests::focused_element(app.channel_dialog_view().unwrap(), opening),
                Some(widget::Id::new("dialog.channel.close"))
            );
            let cancel = if enter {
                let message = super::super::dialog_ui::tests::enter_control(
                    app.channel_dialog_view().unwrap(),
                    State::focus_target(),
                );
                assert!(matches!(message, Message::ChannelUi(Action::Dismiss)));
                message
            } else {
                Message::KeyEvent {
                    event: keyboard::Event::KeyPressed {
                        key: keyboard::Key::Named(keyboard::key::Named::Escape),
                        modified_key: keyboard::Key::Named(keyboard::key::Named::Escape),
                        physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Escape),
                        location: keyboard::Location::Standard,
                        modifiers: keyboard::Modifiers::empty(),
                        text: None,
                        repeat: false,
                    },
                    captured: false,
                }
            };
            drop(app.update(cancel));
            assert!(!app.channel_ui.modal_open());
            assert!(
                app.durable
                    .channels
                    .iter()
                    .any(|current| current.id == channel)
            );
            assert_eq!(app.card_state(card), Some(&original));
            assert!(!app.channel_ui.deletion_running());
        }
    }

    #[test]
    fn primary_channel_guard_and_selection_share_git_directory_fallback() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        let project = ProjectId::new();
        app.sidebar_model.projects.push(SidebarProject::new(
            project,
            RepositoryId::new(),
            "repo".into(),
            "/repo/.git".into(),
            vec![],
            vec![],
        ));
        let channel = ProjectChannel {
            id: ChannelId::new(),
            project_id: project,
            title: String::new(),
            title_source: Default::default(),
            ordinal: 0,
            shared: false,
            cards: vec![],
            focused: None,
        };
        Arc::make_mut(&mut app.durable)
            .channels
            .push(channel.clone());
        let protected = app.project_primary_worktree(project).unwrap();
        drop(app.select_channel(channel.id));
        assert_eq!(protected, Path::new("/repo"));
        assert_eq!(app.active_workspace.as_deref(), Some(protected.as_path()));
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

        app.workspace_model
            .views
            .push(crate::workspace_model::views::View {
                scope: crate::workspace_model::views::Scope::Channel(id),
                revision: 1,
                panes: vec![
                    crate::workspace_model::views::Pane::from_card(
                        &app.durable.cards[0],
                        Some("/repo".into()),
                    )
                    .unwrap(),
                ],
                retired: Vec::new(),
            });
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

    #[test]
    fn channel_title_consumer_requires_primary_chat_and_current_view_despite_local_order() {
        use crate::model::ChannelTitleSource;
        use crate::workspace_model::views::{Pane, Scope, View};
        let mut app = super::super::tests::context_inspector_test_app(true);
        let utility = app.durable.cards[0].id;
        let mut a = app.durable.cards[0].clone();
        a.id = CardId::new();
        a.kind = CardKind::Chat(chat_session());
        let mut b = a.clone();
        b.id = CardId::new();
        b.kind = CardKind::Chat(chat_session());
        let id = ChannelId::new();
        let project = ProjectId::new();
        let durable = Arc::make_mut(&mut app.durable);
        durable.cards.extend([a.clone(), b.clone()]);
        durable.channels.push(ProjectChannel {
            id,
            project_id: project,
            title: String::new(),
            title_source: ChannelTitleSource::Prompt,
            ordinal: 0,
            shared: true,
            cards: vec![utility, a.id, b.id],
            focused: Some(a.id),
        });
        app.workspace_model.views.push(View {
            scope: Scope::Channel(id),
            revision: 1,
            panes: vec![
                Pane::from_card(&a, Some("/repo".into())).unwrap(),
                Pane::from_card(&b, Some("/repo".into())).unwrap(),
            ],
            retired: Vec::new(),
        });
        assert_eq!(
            app.channel_primary_chat(&app.durable.channels[0]),
            Some(a.id)
        );
        assert!(!app.suggest_channel_title(
            b.id,
            "Sibling conversation".into(),
            ChannelTitleSource::Provider
        ));
        assert!(app.suggest_channel_title(
            a.id,
            "Encryption review".into(),
            ChannelTitleSource::Provider
        ));
        assert!(app.suggest_channel_title(
            a.id,
            "Encryption and throughput review".into(),
            ChannelTitleSource::Provider
        ));
        assert!(!app.suggest_channel_title(
            a.id,
            "Encryption and throughput review".into(),
            ChannelTitleSource::Provider
        ));
        // A watcher can update the authoritative view before the local cards
        // are projected. Local stale primary membership alone grants nothing.
        app.workspace_model.views[0].panes.swap(0, 1);
        app.workspace_model.views[0].revision += 1;
        assert!(!app.suggest_channel_title(
            a.id,
            "Delayed old owner".into(),
            ChannelTitleSource::Provider
        ));
        let channel = &mut Arc::make_mut(&mut app.durable).channels[0];
        channel.cards = vec![utility, b.id, a.id];
        channel.title.clear();
        channel.title_source = ChannelTitleSource::Prompt;
        assert!(app.suggest_channel_title(
            b.id,
            "Review the next task".into(),
            ChannelTitleSource::Prompt
        ));
        let channel = &mut Arc::make_mut(&mut app.durable).channels[0];
        channel.title = "My project name".into();
        channel.title_source = ChannelTitleSource::Manual;
        assert!(!app.suggest_channel_title(
            b.id,
            "Late generated name".into(),
            ChannelTitleSource::Provider
        ));
        assert_eq!(app.durable.channels[0].title, "My project name");
    }

    /// Channel rows are rebuilt on every structural update, so the refresh
    /// must stay linear: many projects, channels and cards produce exactly the
    /// rows a per-project scan would, in durable order.
    #[test]
    fn sidebar_channel_rows_at_scale_follow_durable_order_per_project() {
        let mut app = super::super::tests::context_inspector_test_app(true);
        let template = app.durable.cards[0].clone();
        let projects = (0..8).map(|_| ProjectId::new()).collect::<Vec<_>>();
        for (index, project) in projects.iter().enumerate() {
            app.sidebar_model.projects.push(SidebarProject::new(
                *project,
                RepositoryId::new(),
                format!("project {index}"),
                format!("/repo-{index}").into(),
                Vec::new(),
                Vec::new(),
            ));
        }
        let durable = Arc::make_mut(&mut app.durable);
        // Interleave projects so grouping cannot rely on adjacency.
        for ordinal in 0..200 {
            let cards = (0..3)
                .map(|_| {
                    let mut card = template.clone();
                    card.id = CardId::new();
                    durable.cards.push(card.clone());
                    card.id
                })
                .collect::<Vec<_>>();
            durable.channels.push(ProjectChannel {
                title_source: Default::default(),
                id: ChannelId::new(),
                project_id: projects[ordinal % projects.len()],
                title: format!("channel {ordinal}"),
                ordinal: ordinal as u32,
                shared: false,
                focused: cards.first().copied(),
                cards,
            });
        }
        let live = app.durable.channels[7].id;
        app.channel_ui.failed.insert(live, "kept".into());
        app.channel_ui
            .failed
            .insert(ChannelId::new(), "gone".into());

        app.refresh_sidebar_channels();
        for project in &app.sidebar_model.projects {
            let expected = app
                .durable
                .channels
                .iter()
                .filter(|channel| channel.project_id == project.id)
                .map(|channel| (channel.id, channel.title.as_str()))
                .collect::<Vec<_>>();
            let rows = project
                .channels
                .iter()
                .map(|row| (row.id, row.title.as_str()))
                .collect::<Vec<_>>();
            assert_eq!(rows, expected);
            assert!(project.channels.iter().all(|row| row.has_panes));
        }
        assert_eq!(
            app.channel_ui.failed.keys().copied().collect::<Vec<_>>(),
            [live],
            "only live channels keep a failure"
        );

        // A removed channel leaves its project's rows on the next refresh.
        let removed = Arc::make_mut(&mut app.durable).channels.remove(0);
        app.refresh_sidebar_channels();
        let rows = &app.sidebar_model.projects[0].channels;
        assert!(rows.iter().all(|row| row.id != removed.id));
        assert_eq!(rows.len(), 200 / projects.len() - 1);
    }

    /// The chrome pass resolves channel titles in one sweep; it must present a
    /// chat exactly as the single-card lookup does.
    #[test]
    fn chat_chrome_takes_its_channel_title_like_the_single_card_lookup() {
        let mut app = super::super::tests::context_inspector_test_app(true);
        let card = app.workspace.focused().unwrap();
        let other = ChannelId::new();
        let named = ChannelId::new();
        let durable = Arc::make_mut(&mut app.durable);
        let session = chat_session();
        durable.cards[0].title =
            super::super::chat_ui::chat_card_title(session.launch.provider).into();
        durable.cards[0].kind = crate::model::CardKind::Chat(session);
        let project = ProjectId::new();
        // An untitled channel first: the first *titled* channel names the chat.
        for (id, title) in [(other, ""), (named, "Parser rewrite")] {
            durable.channels.push(ProjectChannel {
                title_source: crate::model::ChannelTitleSource::Manual,
                id,
                project_id: project,
                title: title.into(),
                ordinal: 0,
                shared: false,
                cards: vec![card],
                focused: Some(card),
            });
        }
        drop(app.sync_chrome());
        let state = app.card_state(card).unwrap();
        assert_eq!(app.card_display_title(state), "Parser rewrite");
        assert_eq!(app.repository_chrome(state), "Parser rewrite");
        assert_eq!(app.card_chrome[&card].title, "Parser rewrite");

        Arc::make_mut(&mut app.durable).channels[1].title = "Renamed".into();
        drop(app.sync_chrome());
        assert_eq!(app.card_chrome[&card].title, "Renamed");
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

    /// Clicking down the channel list changes where this window is and
    /// nothing else, so the clicks are saved once, after the burst, instead of
    /// each one projecting and copying the whole workspace on the UI thread.
    /// Showing a channel on the way back from a change is still saved at once.
    #[test]
    fn opening_channels_from_the_sidebar_saves_once_after_the_burst() {
        use super::super::terminal_output_tests::persists;
        let (mut app, first, second, _) = channel_with_a_conversation();
        let durable = Arc::make_mut(&mut app.durable);
        let mut other = durable.cards[0].clone();
        other.id = CardId::new();
        other.kind = crate::model::CardKind::Chat(chat_session());
        durable.cards.push(other.clone());
        durable
            .channels
            .iter_mut()
            .find(|channel| channel.id == second)
            .unwrap()
            .cards
            .push(other.id);
        app.refresh_sidebar_channels();
        drop(app.select_channel(first));
        drop(app.update(Message::NavigationSaveDue(app.navigation_save_generation)));

        let saved = persists();
        let mut armed = None;
        for channel in [second, first, second] {
            drop(app.update(Message::Sidebar(SidebarAction::OpenChannel {
                channel_id: channel,
            })));
            assert_eq!(app.durable.active_channel, Some(channel));
            assert_eq!(app.workspace.order().collect::<Vec<_>>().len(), 1);
            assert_eq!(persists(), saved, "a click alone waits for the burst");
            assert!(app.navigation_save_pending);
            // The burst's first click arms the one timer; later clicks don't
            // push it back, so a long burst is still saved within the delay.
            let generation = *armed.get_or_insert(app.navigation_save_generation);
            assert_eq!(app.navigation_save_generation, generation);
        }
        let burst = app.navigation_save_generation;
        drop(app.update(Message::NavigationSaveDue(burst)));
        assert_eq!(persists(), saved + 1, "the burst is saved once");
        assert!(!app.navigation_save_pending);

        // A timer armed for a burst some other save already wrote does not
        // cut the next burst short.
        drop(app.update(Message::Sidebar(SidebarAction::OpenChannel {
            channel_id: first,
        })));
        drop(app.select_channel(second));
        assert_eq!(persists(), saved + 2, "a programmatic switch saves at once");
        drop(app.update(Message::Sidebar(SidebarAction::OpenChannel {
            channel_id: first,
        })));
        let stale = app.navigation_save_generation - 1;
        drop(app.update(Message::NavigationSaveDue(stale)));
        assert_eq!(persists(), saved + 2, "the earlier burst's timer is spent");
        assert!(app.navigation_save_pending);
        drop(app.update(Message::NavigationSaveDue(app.navigation_save_generation)));
        assert_eq!(persists(), saved + 3);
        assert_eq!(app.durable.active_channel, Some(first));
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
        let _ = app.handle_channel_ui(Action::SessionsClosed(
            doomed,
            app.channel_ui.close_operation(doomed),
            Ok(()),
        ));
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
            app.channel_ui.close_operation(doomed),
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
            app.channel_ui.close_operation(doomed),
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
        let _ = app.handle_channel_ui(Action::SessionsClosed(
            doomed,
            app.channel_ui.close_operation(doomed),
            Ok(()),
        ));
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

#[cfg(test)]
mod revival_tests;

#[cfg(test)]
mod close_tests {
    use super::*;
    use crate::agent_execution::{
        Agent, HostedSession, Origin, Placement, Spec, Surface, Target as AgentTarget,
    };

    fn conversation(continuity: crate::chat::NativeContinuity) -> crate::chat::Session {
        let mut session = crate::chat::fresh_session(
            crate::agent::Provider::Claude,
            TerminalId::new(),
            "/repo".into(),
            &crate::account::Account::system(crate::agent::Provider::Claude),
        )
        .unwrap();
        session.native_continuity = continuity;
        session
    }

    fn agent(session: &crate::chat::Session) -> Agent {
        let placement = Placement::Channel(ChannelId::new());
        Agent {
            id: crate::model::AgentId::new(),
            name: None,
            origin: Origin::Automation {
                id: uuid::Uuid::new_v4(),
            },
            spec: Spec {
                provider: crate::agent::Provider::Claude,
                account: None,
                target: AgentTarget {
                    repository: "/repo".into(),
                    directory: "/repo".into(),
                    placement: Some(placement.clone()),
                },
                surface: Surface::Chat,
                model: None,
                permission_mode: None,
                reasoning_effort: None,
            },
            placement,
            session: Some(HostedSession::Chat(session.clone())),
            console: None,
            created_ms: 0,
            closed: false,
        }
    }

    /// A conversation a stopped daemon left without a live actor still runs
    /// as its open agent, so closing it retires that agent. One no agent runs
    /// as, and that nothing retained can own, asks the daemon nothing it
    /// could only refuse.
    #[test]
    fn a_conversation_an_open_agent_runs_as_is_closed_through_the_daemon() {
        for continuity in [
            crate::chat::NativeContinuity::ReconnectOnly,
            crate::chat::NativeContinuity::RetainedClaudeActor { protocol: 2 },
        ] {
            let session = conversation(continuity);
            assert!(!may_have_retained_owner(&session));
            let mut bound = agent(&session);
            assert!(bound_to_open_agent(&session, &[bound.clone()]));

            let other = conversation(crate::chat::NativeContinuity::ReconnectOnly);
            assert!(!bound_to_open_agent(&other, &[bound.clone()]));
            assert!(!bound_to_open_agent(&session, &[]));
            bound.closed = true;
            assert!(!bound_to_open_agent(&session, &[bound]));
        }
    }
}
