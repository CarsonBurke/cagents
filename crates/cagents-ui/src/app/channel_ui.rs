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

/// Fresh replaces this exact conversation, rather than whichever conversation
/// currently occupies its native ID. A changed owner must be reconciled before
/// Fresh can be attempted again; inventory alone cannot authorize that change.
pub(super) fn close_captured_chat(source: crate::chat::Session) -> Result<(), String> {
    let socket = crate::terminal::ipc::daemon_socket_path().map_err(|error| error.to_string())?;
    let Some(mut client) = connect(&socket)? else {
        return no_daemon_captured_close_result(&source);
    };
    close_captured_chat_with(&mut client, &source)
}

fn no_daemon_captured_close_result(source: &crate::chat::Session) -> Result<(), String> {
    if source.terminal.is_some() {
        return Err(
            "Reconnect the daemon to confirm the retained native provider has stopped".into(),
        );
    }
    no_daemon_close_result(&[CardKind::Chat(source.clone())])
}

fn close_captured_chat_with(
    client: &mut crate::control::ControlClient,
    source: &crate::chat::Session,
) -> Result<(), String> {
    let chats = client.list_chats().map_err(|error| error.to_string())?;
    let live = chats.iter().find(|live| live.id == source.id);
    if live.is_some_and(|live| live != source) {
        return Err(captured_chat_changed());
    }
    let mut agents = None;
    // Closing an open agent's conversation retires the agent and its pane,
    // which leaves Fresh no card to start in. Refuse before closing anything.
    if captured_open_agent(source, read_open_agents(&mut agents, client)?)? {
        return Err(agent_owned_conversation());
    }
    if live.is_some() {
        // The attachment must still be the captured descriptor after inventory
        // was read. In particular, never close an ID-only handle or derive a
        // paired terminal from a replacement occupying the same native ID.
        client
            .close_chat_with_snapshot(source.clone())
            .map_err(|error| error.reason())?;
        agents = None;
        if captured_open_agent(source, read_open_agents(&mut agents, client)?)? {
            return Err(agent_owned_conversation());
        }
    } else if may_have_retained_owner(source) || source.terminal.is_some() {
        // Actor absence is not a retained-owner or paired-terminal receipt.
        client
            .close_retained_chat(source.clone())
            .map_err(|error| error.reason())?;
    }
    Ok(())
}

fn captured_open_agent(
    source: &crate::chat::Session,
    agents: &[crate::agent_execution::Agent],
) -> Result<bool, String> {
    let mut bound = false;
    for agent in agents.iter().filter(|agent| !agent.closed) {
        if let Some(crate::agent_execution::HostedSession::Chat(session)) = &agent.session
            && session.id == source.id
        {
            if session != source {
                return Err(captured_chat_changed());
            }
            bound = true;
        }
    }
    Ok(bound)
}

fn agent_owned_conversation() -> String {
    "An agent runs this conversation, so it can't start a new one in its place. Close it instead."
        .into()
}

fn captured_chat_changed() -> String {
    "The hosted chat does not match the exact saved conversation".into()
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
        let (_, open_agents) = client
            .read_workspace_agents()
            .map_err(|error| error.to_string())?;
        *agents = Some(
            open_agents
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

/// What made an empty channel a candidate for retirement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Retirement {
    /// Its last card closed here, which was the person's own act.
    Closed,
    /// It was found empty: when the person left it, or by the startup
    /// sweep. Nothing they did closed it, so a name they gave keeps it.
    Unattended,
}

/// The pending deletion, held while its confirmation is on screen.
#[derive(Debug, Default)]
pub struct State {
    dialog: Option<Deletion>,
    /// A confirmed deletion waiting while the user saves or discards the
    /// unsaved editors it named, with the consent they gave.
    resolving: Option<(ChannelId, Consent)>,
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
    /// Channels the person renamed in this window. A name is never taken back
    /// automatically, so this stays true once the registry has the rename.
    named: HashSet<ChannelId>,
    reopening: HashMap<ChannelId, PendingReopen>,
    /// Emptied channels waiting for their view publication to settle, by the
    /// incarnation the close saw, with what made each a candidate. The
    /// registry checks that incarnation and the settled view revision before
    /// it retires one.
    pub(super) retire_candidates: HashMap<crate::workspace_model::ChannelIncarnation, Retirement>,
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
    home: crate::workspace_model::ChannelIncarnation,
    sent: std::time::Instant,
    intents: Vec<Reopen>,
}

#[derive(Debug)]
pub(super) enum Reopen {
    Conversation {
        entry: Box<crate::persistence::SavedConversation>,
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

/// What a pane is a view of, as far as deleting it is concerned: a pane that
/// starts showing another session is not the pane the user agreed to close.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PaneSource {
    Terminal(crate::model::TerminalId),
    Chat(crate::model::TerminalId),
    Other(std::mem::Discriminant<CardKind>),
}

impl PaneSource {
    fn of(kind: &CardKind) -> Self {
        match kind {
            // Recovery can give a terminal a new identity while it stays the
            // same shell; its origin is what stays fixed.
            CardKind::Terminal(session) => {
                Self::Terminal(session.recovery_origin.unwrap_or(session.id))
            }
            CardKind::Chat(session) => Self::Chat(session.id),
            other => Self::Other(std::mem::discriminant(other)),
        }
    }
}

/// Everything deleting a channel would close or lose, as the user was shown
/// it. A deletion only goes ahead while this still covers the channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Consent {
    /// The channel's open panes, by identity, with what each shows. Their
    /// order is presentation, not part of what is deleted.
    panes: Vec<(CardId, PaneSource)>,
    /// Open chats and terminals among them.
    conversations: usize,
    /// Editors whose work exists nowhere else yet, by the name shown for them.
    unsaved: Vec<(CardId, String)>,
}

impl Consent {
    pub(super) fn conversations(&self) -> usize {
        self.conversations
    }

    /// Whether deleting the channel as it is now loses nothing beyond what
    /// this consent named: the same panes showing the same sources, and no
    /// unsaved editor it did not list.
    pub(super) fn covers(&self, current: &Self) -> bool {
        self.panes == current.panes
            && current
                .unsaved
                .iter()
                .all(|(card, _)| self.unsaved.iter().any(|(named, _)| named == card))
    }
}

#[derive(Debug, Clone)]
struct Deletion {
    channel: ChannelId,
    title: String,
    /// What the dialog names, fixed when it opened. Confirming deletes no
    /// more than this.
    consent: Consent,
    /// The channel changed after the dialog was first shown; this is the
    /// fresh account of it.
    changed: bool,
    /// Why the last deletion of this channel failed, when the dialog is
    /// opened again after it. Confirming retries it.
    failure: Option<String>,
}

impl State {
    /// The channels whose last change failed, for the projection check.
    #[cfg(any(test, feature = "development"))]
    pub(super) fn projection_view(&self) -> String {
        super::projection_oracle::sorted_entries(&self.failed)
    }

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
        super::dialog_ui::Question::DeleteChannel.initial_focus(
            widget::Id::new("dialog.channel.close"),
            widget::Id::new("dialog.channel.confirm"),
        )
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
    /// workspace saves it. Only a channel this window just `created` is known
    /// to be empty before its shared view loads.
    fn open_channel_with_default(&mut self, id: ChannelId, created: bool) -> Task<Message> {
        let scope = self.navigate_to_channel(id);
        if self.durable.active_channel != Some(id) {
            return scope;
        }
        let default = if created {
            self.ensure_new_channel_default_pane()
        } else {
            self.ensure_default_pane()
        };
        Task::batch([scope, default])
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
            // Already under way, so a refusal here is a failed deletion the
            // channel's row offers to retry, not a deletion never started.
            if let Some(blocker) = self
                .durable
                .channels
                .iter()
                .find(|channel| channel.id == id)
                .and_then(|channel| self.channel_deletion_blocker(&channel.cards))
            {
                self.status = Some(TransientNotice::warning(blocker));
                self.channel_ui.failed.insert(id, blocker.into());
                self.refresh_sidebar_channels();
                return Task::none();
            }
            return self.delete_channel(id);
        }
        if let Action::SessionsClosed(id, operation, result) = action {
            if self
                .channel_ui
                .closing
                .get(&id)
                .is_none_or(|attempt| attempt.operation != operation)
            {
                return Task::none();
            }
            let attempt = self.channel_ui.closing.remove(&id).unwrap();
            self.channel_ui.deleting.remove(&id);
            let result = result
                .and_then(|()| {
                    self.channel_close_targets_current(id, &attempt).then_some(()).ok_or_else(|| {
                        "The channel's panes changed while sessions were closing; review them before deleting again".into()
                    })
                })
                .and_then(|()| {
                    self.unsaved_channel_editors(&attempt.members)
                        .is_empty()
                        .then_some(())
                        .ok_or_else(|| {
                            "An editor in the channel has unsaved changes; save or discard them before deleting again".into()
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
                let scope = self.open_channel_with_default(id, true);
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
            Action::Open(id) => self.open_channel_with_default(id, false),
            Action::Delete(id) => self.begin_channel_deletion(id, false),
            Action::RequestDelete(id) => self.begin_channel_deletion(id, true),
            Action::ConfirmDelete => {
                let Some(deletion) = self.channel_ui.dialog.take() else {
                    return Task::none();
                };
                self.delete_consented_channel(deletion.channel, &deletion.consent)
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
        let home = crate::workspace_model::ChannelIncarnation {
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
                        *entry, prepared, generation, replace, same_home,
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
                    .retain(|candidate, _| candidate.channel != id);
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

    #[cfg(all(test, not(feature = "integrated-browser")))]
    pub(super) fn remember_channel_name_for_test(&mut self, id: ChannelId, title: String) {
        self.remember_channel_name(id, title, crate::model::ChannelTitleSource::Provider);
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
        self.capture_retire_candidates(&emptied, Retirement::Closed);
        // The closing card is still open here; settle after its close lands.
        // A published pane is still in its view until the registry answers
        // the removal, which keeps the channel waiting for that answer.
        cosmic::task::message(Message::ChannelUi(Action::SettleRetirements))
    }

    /// Retires channel `left`, which this window just navigated away from,
    /// when it holds nothing and is not named, as closing its last card would
    /// have. A channel can reach that state without one of its cards closing
    /// here: a pane moved out, a discarded composer, a peer's close, or a
    /// restore.
    pub(super) fn retire_left_channel(&mut self, left: Option<ChannelId>) {
        let Some(left) = left.filter(|&id| self.durable.active_channel != Some(id)) else {
            return;
        };
        if self.channel_retires_unattended(left) {
            self.capture_retire_candidates(&[left], Retirement::Unattended);
            self.retire_settled_empty_channels();
        }
    }

    /// Asks once, after this window's restore settled and it joined the
    /// registry, to retire every unnamed channel it is not showing that holds
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
            .filter(|&id| {
                self.durable.active_channel != Some(id) && self.channel_retires_unattended(id)
            })
            .collect::<Vec<_>>();
        if empty.is_empty() {
            return;
        }
        tracing::debug!(
            count = empty.len(),
            "retiring channels found empty at startup"
        );
        self.capture_retire_candidates(&empty, Retirement::Unattended);
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

    /// Whether `id` may retire without the person closing anything in it:
    /// it holds nothing, and it is not named. A name is something they put
    /// there, so leaving a named channel keeps it, even one they have not
    /// used yet. An untitled channel has nothing of theirs to lose; the
    /// registry clears a title its conversation suggested when that
    /// conversation leaves.
    fn channel_retires_unattended(&self, id: ChannelId) -> bool {
        !self.channel_is_named(id) && self.channel_holds_nothing(id)
    }

    /// Whether `id` carries a name given to it rather than one suggested by
    /// its conversation: by a rename, or when it was made with a title.
    ///
    /// A rename made here counts before the registry confirms it. Until
    /// then, a model the registry sent before the rename shows the old title.
    pub(super) fn channel_is_named(&self, id: ChannelId) -> bool {
        self.channel_ui.named.contains(&id)
            || self.durable.channels.iter().any(|channel| {
                channel.id == id
                    && channel.title_source == crate::model::ChannelTitleSource::Manual
                    && !channel.title.is_empty()
            })
    }

    /// Records that the person named channel `id` in this window.
    pub(super) fn note_channel_named(&mut self, id: ChannelId) {
        self.channel_ui.named.insert(id);
    }

    /// Records that channel `id` went back to its automatic name.
    pub(super) fn note_channel_unnamed(&mut self, id: ChannelId) {
        self.channel_ui.named.remove(&id);
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
    /// it bring it back. A close here outweighs finding the channel empty.
    fn capture_retire_candidates(&mut self, ids: &[ChannelId], retirement: Retirement) {
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
            let home = crate::workspace_model::ChannelIncarnation {
                registry: self.workspace_model.identity,
                channel: id,
                creation_ordinal: channel.ordinal,
            };
            home.is_confirmed().then_some(home)
        });
        for home in captured.collect::<Vec<_>>() {
            let candidate = self
                .channel_ui
                .retire_candidates
                .entry(home)
                .or_insert(retirement);
            if retirement == Retirement::Closed {
                *candidate = Retirement::Closed;
            }
        }
    }

    /// View publication supplies the settled revision. The registry checks
    /// that exact revision and the channel's incarnation before retiring it.
    pub(super) fn retire_settled_empty_channels(&mut self) {
        let candidates = self
            .channel_ui
            .retire_candidates
            .iter()
            .map(|(&home, &retirement)| (home, retirement))
            .collect::<Vec<_>>();
        for (captured, retirement) in candidates {
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
            // A name given since it was found empty keeps it too.
            if self.channel_holds_unpublished_work(channel)
                || self.channel_awaits_session(channel, view)
                || (retirement == Retirement::Unattended && self.channel_is_named(channel))
            {
                self.channel_ui.retire_candidates.remove(&captured);
                continue;
            }
            let edit = ModelEdit::RetireEmptyChannel {
                home: captured,
                expected_view_revision: view.revision,
            };
            if self.workspace_model_tx.try_send(edit.into()).is_ok() {
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
        let consent = self.channel_deletion_consent(channel);
        if consent.conversations == 0 && consent.unsaved.is_empty() && !always_confirm {
            return self.delete_channel(id);
        }
        self.open_channel_deletion_dialog(id, consent, false)
    }

    /// Shows the delete confirmation for `consent`, opening on Cancel.
    fn open_channel_deletion_dialog(
        &mut self,
        id: ChannelId,
        consent: Consent,
        changed: bool,
    ) -> Task<Message> {
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
        self.channel_ui.dialog = Some(Deletion {
            channel: id,
            title,
            consent,
            changed,
            failure: self.channel_ui.failed.get(&id).cloned(),
        });
        iced::widget::operation::focus(State::focus_target()).map(cosmic::Action::App)
    }

    /// Whether this editor holds work that closing it would lose: unsaved
    /// changes, a save still in flight, or a save whose outcome is unresolved.
    pub(super) fn editor_holds_unsaved_work(&self, card: CardId) -> bool {
        matches!(self.cards.get(&card), Some(CardRuntime::Repository(runtime)) if runtime.blocks_navigation())
    }

    /// The editors among `cards` that deleting them would lose work in. Every
    /// way of deleting a channel asks this before it closes anything.
    pub(super) fn unsaved_channel_editors(&self, cards: &[CardId]) -> Vec<CardId> {
        cards
            .iter()
            .copied()
            .filter(|card| self.editor_holds_unsaved_work(*card))
            .collect()
    }

    /// What deleting `channel` now would close and lose, for a confirmation
    /// to show and later hold the deletion to.
    pub(super) fn channel_deletion_consent(&self, channel: &ProjectChannel) -> Consent {
        let mut panes: Vec<_> = channel
            .cards
            .iter()
            .filter_map(|card| self.card_state(*card))
            .filter(|state| state.existence.is_open() && !state.kind.is_utility())
            .map(|state| (state.id, PaneSource::of(&state.kind)))
            .collect();
        panes.sort_by_key(|(card, _)| *card);
        let unsaved = self
            .unsaved_channel_editors(&channel.cards)
            .into_iter()
            .map(|card| (card, self.unsaved_editor_name(card)))
            .collect();
        Consent {
            panes,
            conversations: self.channel_conversation_count(channel),
            unsaved,
        }
    }

    /// The file an unsaved editor has open, as its pane would name it.
    pub(super) fn unsaved_editor_name(&self, card: CardId) -> String {
        match self.cards.get(&card) {
            Some(CardRuntime::Repository(runtime)) => runtime
                .selected_path()
                .map(|path| path.display().to_string()),
            _ => None,
        }
        .or_else(|| self.card_state(card).map(|state| state.title.clone()))
        .unwrap_or_else(|| "An editor".into())
    }

    /// Goes ahead with a deletion the user confirmed, provided the channel
    /// still holds only what they were shown. Otherwise the confirmation
    /// comes back with what it holds now, and nothing is closed.
    fn delete_consented_channel(&mut self, id: ChannelId, consent: &Consent) -> Task<Message> {
        let Some(current) = self
            .durable
            .channels
            .iter()
            .find(|channel| channel.id == id)
            .map(|channel| self.channel_deletion_consent(channel))
        else {
            return Task::none();
        };
        if !consent.covers(&current) {
            return self.open_channel_deletion_dialog(id, current, true);
        }
        if current.unsaved.is_empty() {
            return self.delete_channel(id);
        }
        // Each unsaved editor is offered Save first, as closing it alone
        // would; the deletion continues once every one is resolved.
        let editors = current.unsaved.iter().map(|(card, _)| *card).collect();
        self.channel_ui.resolving = Some((id, consent.clone()));
        self.begin_dirty_close(DirtyCloseIntent::Channel(id), editors)
    }

    /// The unsaved editors of a confirmed deletion are saved or discarded.
    pub(super) fn resume_channel_deletion(&mut self, id: ChannelId) -> Task<Message> {
        match self.channel_ui.resolving.take() {
            Some((resolving, consent)) if resolving == id => {
                self.delete_consented_channel(id, &consent)
            }
            other => {
                self.channel_ui.resolving = other;
                Task::none()
            }
        }
    }

    pub(super) fn cancel_channel_deletion(&mut self, id: ChannelId) {
        if self
            .channel_ui
            .resolving
            .as_ref()
            .is_some_and(|(resolving, _)| *resolving == id)
        {
            self.channel_ui.resolving = None;
        }
    }

    /// Why `cards` cannot be deleted with their channel yet, if anything.
    fn channel_deletion_blocker(&self, cards: &[CardId]) -> Option<&'static str> {
        if cards.iter().any(|card| self.pane_transfer_pending(*card)) {
            Some("Wait for the pane move to finish before deleting this channel.")
        } else if !self.unsaved_channel_editors(cards).is_empty() {
            Some("Save or discard the unsaved editor changes in this channel before deleting it.")
        } else {
            None
        }
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

    /// The saved sessions deleting a channel of `cards` asks the daemon to
    /// close. A fenced pane's saved session is unconfirmed, so nothing is
    /// asked of its agent: deleting the channel retires only the pane.
    pub(super) fn channel_sessions_to_close(&self, cards: &[CardId]) -> Vec<CardKind> {
        self.durable
            .cards
            .iter()
            .filter(|card| {
                cards.contains(&card.id)
                    && card.existence.is_open()
                    && !self.shared_views.quarantined.contains(&card.id)
            })
            .map(|card| card.kind.clone())
            .collect()
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
        if let Some(blocker) = self.channel_deletion_blocker(&cards) {
            self.status = Some(TransientNotice::warning(blocker));
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
        let sessions = self.channel_sessions_to_close(&cards);
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
            .filter(|card| {
                cards.contains(&card.id)
                    && card.existence.is_open()
                    && !self.shared_views.quarantined.contains(&card.id)
            })
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
        self.forget_view_seed(&crate::workspace_model::views::Scope::Channel(id));
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
                    || self.editor_holds_unsaved_work(card))
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
            super::notice_body(match dialog.consent.conversations {
                0 => format!("Delete \"{}\" from this workspace?", dialog.title),
                1 => format!("Delete \"{}\" and close its conversation?", dialog.title),
                chats => format!(
                    "Delete \"{}\" and close its {chats} conversations?",
                    dialog.title
                ),
            }),
        ];
        if let Some(unsaved) = unsaved_editors_notice(&dialog.consent.unsaved) {
            rows.push(
                widget::text::body(unsaved)
                    .class(cosmic::theme::Text::Custom(design::warning_text))
                    .wrapping(iced::widget::text::Wrapping::WordOrGlyph)
                    .width(Length::Fill)
                    .into(),
            );
        }
        if dialog.changed {
            rows.push(
                widget::text::body(
                    "The channel changed while this was open. Review what it holds now.",
                )
                .class(cosmic::theme::Text::Custom(design::warning_text))
                .wrapping(iced::widget::text::Wrapping::WordOrGlyph)
                .width(Length::Fill)
                .into(),
            );
        }
        if let Some(failure) = &dialog.failure {
            rows.push(super::notice_body(format!(
                "The last attempt failed: {failure}"
            )));
        }
        let actions = super::dialog_ui::confirmation_actions(
            crate::ui_controls::button::secondary("Cancel")
                .id(widget::Id::new("dialog.channel.close"))
                .on_press(Message::ChannelUi(Action::Dismiss)),
            crate::ui_controls::button::destructive("Delete channel")
                .instead("Retry delete", dialog.failure.is_some())
                .id(widget::Id::new("dialog.channel.confirm"))
                .on_press(Message::ChannelUi(Action::ConfirmDelete)),
        );
        // A long failure or list of unsaved editors scrolls; the answers stay
        // in reach.
        Some(super::dialog_ui::confirmation_surface(
            super::dialog_ui::Question::DeleteChannel,
            widget::column::with_children(rows).spacing(space::M),
            actions,
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

    /// Whether `card` is the primary chat of a channel with no name, which
    /// its conversation may fill. An empty title is unnamed whatever its
    /// source: a new channel starts empty and manual, and the reader names a
    /// channel only with a title (see `channel_is_named`).
    pub(super) fn channel_lacks_name(&self, card: CardId) -> bool {
        self.durable.channels.iter().any(|channel| {
            channel.title.is_empty() && self.channel_primary_chat(channel) == Some(card)
        })
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
        self.refresh_attention_projection();
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
        // The rows are compared through a shared borrow and written only
        // where they differ: a mutable borrow takes a new revision, and the
        // navigation list deep-compares every project on a frame after one.
        // Most projections change no row, so most take none.
        let mut changed_rows = Vec::new();
        let mut row_counts = Vec::new();
        for (position, project) in self.sidebar_model.projects.iter().enumerate() {
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
                let mut members = sidebar::ChannelMembers::default();
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
                    // A live agent shows as its provider; an open shell with no
                    // agent as a terminal. A chat whose provider has not
                    // started, or has ended, has no one to show.
                    match (state.card_activity(*id), &card.kind) {
                        // A native chat reports an unknown state once its
                        // provider has exited, and before it has started.
                        (Some(activity), CardKind::Chat(_))
                            if activity.state == crate::agent::ActivityState::Unknown => {}
                        (Some(activity), _) => members.add_agent(
                            activity.provider,
                            sidebar::ChannelMemberState::of(activity.state, attention.limited != 0),
                        ),
                        // A shell that ended, or whose runtime failed, has
                        // nobody in it.
                        (None, CardKind::Terminal(session))
                            if !super::terminal_ended(session)
                                && !matches!(
                                    session.continuity,
                                    TerminalContinuity::Failed { .. }
                                ) =>
                        {
                            match crate::agent::Provider::recognize_launch(&session.launch) {
                                // An agent launched in the terminal that has
                                // yet to report what it is doing.
                                Some(provider) => members
                                    .add_agent(provider, sidebar::ChannelMemberState::Running),
                                None => members.add_terminal(),
                            }
                        }
                        (None, _) => {}
                    }
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
                        && row.members == members
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
                    .with_members(members)
                    .with_attention(attention);
                    row.deletion = deletion;
                    row = row.with_notification_preview(notification_preview.cloned());
                    changed_rows.push((position, index, row));
                }
                index += 1;
            }
            if index != project.channels.len() {
                row_counts.push((position, index));
            }
        }
        if changed_rows.is_empty() && row_counts.is_empty() {
            return;
        }
        let projects = &mut *self.sidebar_model.projects;
        // Ascending per project, so a row past the end is always the next one.
        for (position, index, row) in changed_rows {
            let channels = &mut projects[position].channels;
            if index < channels.len() {
                channels[index] = row;
            } else {
                channels.push(row);
            }
        }
        for (position, count) in row_counts {
            projects[position].channels.truncate(count);
        }
    }
}

impl Cagents {
    /// Folds attention into the rows the sidebar draws: counts first, then
    /// the pane rows they give. Every projection goes through here, so
    /// no caller can leave the pane rows behind the counts.
    pub(super) fn refresh_attention_projection(&mut self) {
        self.sidebar_state
            .refresh_attention_projection(&self.durable, &self.sidebar_model.projects);
        self.refresh_attention_card_rows();
    }

    /// The sidebar's pane rows, titled as their panes are beside their
    /// place: one per open conversation outside every channel. Every title
    /// report comes through here, and most change no row: those compare the
    /// rows in place and stop, and the rest rebuild only the projects whose
    /// rows changed.
    pub(super) fn refresh_attention_card_rows(&mut self) {
        let stale = self.sidebar_model.stale_panes(&self.sidebar_state, |id| {
            self.card_state(id).map(|card| self.pane_row_title(card))
        });
        if stale.is_empty() {
            return;
        }
        let mut titles = self
            .sidebar_state
            .pane_cards()
            .filter_map(|id| {
                let card = self.card_state(id)?;
                Some((id, self.pane_row_title(card).to_owned()))
            })
            .collect::<HashMap<_, _>>();
        self.sidebar_model
            .set_panes(&self.sidebar_state, &stale, |id| titles.remove(&id));
    }
}

/// What the delete confirmation says about unsaved editors, naming each.
fn unsaved_editors_notice(unsaved: &[(CardId, String)]) -> Option<String> {
    let names = unsaved
        .iter()
        .map(|(_, name)| format!("\"{name}\""))
        .collect::<Vec<_>>();
    let list = match names.as_slice() {
        [] => return None,
        [one] => {
            return Some(format!(
                "{one} has unsaved changes. You can save or discard them before the channel is deleted."
            ));
        }
        [first, second] => format!("{first} and {second}"),
        [rest @ .., last] => format!("{}, and {last}", rest.join(", ")),
    };
    Some(format!(
        "{list} have unsaved changes. You can save or discard each before the channel is deleted."
    ))
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
pub(in crate::app) mod tests {
    use super::*;
    use crate::sidebar::RowDeletion;

    fn captured_home() -> crate::workspace_model::ChannelIncarnation {
        crate::workspace_model::ChannelIncarnation {
            registry: uuid::Uuid::new_v4(),
            channel: ChannelId::new(),
            creation_ordinal: 4,
        }
    }

    fn model_with_empty_channel(
        home: crate::workspace_model::ChannelIncarnation,
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
    fn deferred_empty_retirement_never_rebinds_to_replacement_registry_or_channel_incarnation() {
        let original = captured_home();
        for current in [
            crate::workspace_model::ChannelIncarnation {
                registry: uuid::Uuid::new_v4(),
                ..original
            },
            crate::workspace_model::ChannelIncarnation {
                creation_ordinal: original.creation_ordinal + 1,
                ..original
            },
            original,
        ] {
            let mut app = super::super::tests::context_inspector_test_app(false);
            let (send, received) = test_model_channel();
            app.workspace_model_tx = send;
            app.workspace_model = model_with_empty_channel(current);
            app.channel_ui
                .retire_candidates
                .insert(original, Retirement::Closed);
            app.retire_settled_empty_channels();
            if current == original {
                assert_eq!(
                    received.try_recv().unwrap(),
                    ModelEdit::RetireEmptyChannel {
                        home: original,
                        expected_view_revision: 7,
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
            app.workspace_model =
                model_with_empty_channel(crate::workspace_model::ChannelIncarnation {
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
                app.channel_ui.retire_candidates.contains_key(&original),
                ordinal != crate::workspace_model::PENDING_ORDINAL
            );
        }
    }

    /// Why the last deletion failed can run long; it scrolls, and Cancel and
    /// Retry stay in reach at the scaled minimum window.
    #[test]
    fn deletion_answers_stay_in_reach_beside_a_long_failure() {
        use super::super::dialog_ui::tests::{assert_actions_stay_in_reach, long_message};
        let (mut app, channel, _survivor, _card) = channel_with_a_conversation();
        drop(app.handle_channel_ui(Action::Delete(channel)));
        app.channel_ui.dialog.as_mut().unwrap().failure = Some(long_message());
        assert_actions_stay_in_reach(
            || super::super::dialog_ui::overlay(app.channel_dialog_view().unwrap(), None, 1.0),
            &["dialog.channel.close", "dialog.channel.confirm"].map(widget::Id::new),
        );
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
        chat.terminal = Some(Box::new(source.clone()));
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
    fn a_projection_that_changes_no_channel_row_keeps_the_sidebar_revision() {
        use crate::sidebar::Revisioned;

        let mut app = super::super::tests::context_inspector_test_app(true);
        let card = app.workspace.focused().unwrap();
        let project = ProjectId::new();
        let id = ChannelId::new();
        Arc::make_mut(&mut app.durable)
            .channels
            .push(ProjectChannel {
                title_source: Default::default(),
                id,
                project_id: project,
                title: "Parser".into(),
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
        drop(app.project_workspace(false));
        let settled = Revisioned::revision(&app.sidebar_model.projects);

        drop(app.project_workspace(false));
        app.refresh_sidebar_channels();
        assert_eq!(
            Revisioned::revision(&app.sidebar_model.projects),
            settled,
            "rows that are already current take no new revision"
        );

        Arc::make_mut(&mut app.durable).channels[0].title = "Lexer".into();
        drop(app.project_workspace(false));
        assert_ne!(
            Revisioned::revision(&app.sidebar_model.projects),
            settled,
            "a changed row takes a new revision"
        );
        assert_eq!(app.sidebar_model.projects[0].channels[0].title, "Lexer");

        Arc::make_mut(&mut app.durable).channels.clear();
        let renamed = Revisioned::revision(&app.sidebar_model.projects);
        drop(app.project_workspace(false));
        assert_ne!(
            Revisioned::revision(&app.sidebar_model.projects),
            renamed,
            "a removed row takes a new revision"
        );
        assert!(app.sidebar_model.projects[0].channels.is_empty());
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
        let expected =
            crate::workspace_model::channel_title_from_prompt(&prompt).expect("prompt has a title");
        let target = super::super::chat_ui::Target {
            card,
            generation: 0,
        };
        app.name_channel_from_prompt(target, &prompt);
        assert_eq!(app.durable.channels[0].title, expected);
        assert_eq!(app.sidebar_model.projects[0].channels[0].title, expected);

        let durable = Arc::make_mut(&mut app.durable);
        durable.cards[0].title = "A later conversation title".into();
        app.name_channel_from_prompt(target, "A later prompt must not replace it");
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

    /// The row counts who is at work in the channel: each live
    /// agent by its provider, working first, then each open shell. A chat
    /// whose provider has not started shows nothing, and an ended shell drops
    /// out, as does an agent whose activity ends.
    #[test]
    fn a_channel_row_shows_its_live_agents_then_its_shells() {
        use crate::agent::{Activity, ActivityState, Provider};
        let mut app = super::super::tests::context_inspector_test_app(true);
        let project = ProjectId::new();
        let durable = Arc::make_mut(&mut app.durable);
        let shell = durable.cards[0].id;
        let [idle, working, unstarted] = [CardId::new(), CardId::new(), CardId::new()];
        for id in [idle, working, unstarted] {
            let mut chat = durable.cards[0].clone();
            chat.kind = crate::model::CardKind::Chat(chat_session());
            chat.id = id;
            durable.cards.push(chat);
        }
        durable.channels.push(ProjectChannel {
            title_source: Default::default(),
            id: ChannelId::new(),
            project_id: project,
            title: "Investigation".into(),
            ordinal: 0,
            shared: false,
            cards: vec![shell, idle, working, unstarted],
            focused: Some(idle),
        });
        app.sidebar_model.projects.push(SidebarProject::new(
            project,
            RepositoryId::new(),
            "project".into(),
            "/repo".into(),
            Vec::new(),
            Vec::new(),
        ));
        app.sidebar_state.set_card_activity(
            idle,
            Activity {
                provider: Provider::Codex,
                state: ActivityState::Idle,
            },
        );
        app.sidebar_state.set_card_activity(
            working,
            Activity {
                provider: Provider::Claude,
                state: ActivityState::Working,
            },
        );
        // What a native chat reports before its provider starts and after it
        // exits.
        app.sidebar_state.set_card_activity(
            unstarted,
            Activity {
                provider: Provider::Claude,
                state: ActivityState::Unknown,
            },
        );

        app.refresh_sidebar_channels();
        let row = &app.sidebar_model.projects[0].channels[0];
        assert_eq!(
            row.members.description(),
            "Claude Code, working; Codex, idle; Terminal"
        );
        assert!(
            row.tooltip_for_test()
                .contains("Claude Code, working; Codex, idle; Terminal")
        );
        assert!(
            row.accessible_name_for_test()
                .contains("Claude Code, working; Codex, idle; Terminal")
        );

        let durable = Arc::make_mut(&mut app.durable);
        let crate::model::CardKind::Terminal(session) = &mut durable.cards[0].kind else {
            panic!("the fixture's first card is a shell");
        };
        // A shell whose runtime failed has nobody in it.
        session.continuity = crate::model::TerminalContinuity::Failed {
            typed_error: crate::model::RuntimeError {
                code: "spawn".into(),
                message: "fixture".into(),
            },
            retryable: true,
        };
        app.refresh_sidebar_channels();
        assert_eq!(
            app.sidebar_model.projects[0].channels[0]
                .members
                .description(),
            "Claude Code, working; Codex, idle"
        );

        let durable = Arc::make_mut(&mut app.durable);
        let crate::model::CardKind::Terminal(session) = &mut durable.cards[0].kind else {
            panic!("the fixture's first card is a shell");
        };
        session.continuity = crate::model::TerminalContinuity::Exited {
            last_exit: super::super::terminal_exit(0, None),
        };
        app.sidebar_state.clear_agent_activity(working);
        app.refresh_sidebar_channels();
        let row = &app.sidebar_model.projects[0].channels[0];
        assert_eq!(row.members.description(), "Codex, idle");

        app.sidebar_state.clear_agent_activity(idle);
        app.refresh_sidebar_channels();
        let row = &app.sidebar_model.projects[0].channels[0];
        assert!(row.members.is_empty(), "the channel keeps its glyph");
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
        assert_eq!(
            app.channel_ui
                .dialog
                .as_ref()
                .unwrap()
                .consent
                .conversations,
            1
        );
        assert!(app.durable.channels.iter().any(|channel| channel.id == id));
    }

    /// `channel_with_a_conversation`, plus an editor pane in the doomed
    /// channel holding `runtime`'s work.
    fn channel_with_an_editor(
        runtime: impl FnOnce(CardId) -> crate::repository_widget::RepositoryRuntime,
    ) -> (Cagents, ChannelId, CardId, CardId) {
        let (mut app, doomed, _survivor, conversation) = channel_with_a_conversation();
        let editor = CardId::new();
        let durable = Arc::make_mut(&mut app.durable);
        let mut state = durable.cards[0].clone();
        state.id = editor;
        state.title = "project".into();
        state.kind = CardKind::Repository(crate::model::RepositoryView {
            project: durable.channels[0].project_id,
            repository: "/repo/.git".into(),
            worktree: "/repo".into(),
            selected_file: None,
        });
        durable.cards.push(state);
        durable
            .channels
            .iter_mut()
            .find(|channel| channel.id == doomed)
            .unwrap()
            .cards
            .push(editor);
        app.cards
            .insert(editor, CardRuntime::Repository(Box::new(runtime(editor))));
        (app, doomed, conversation, editor)
    }

    fn channel_exists(app: &Cagents, id: ChannelId) -> bool {
        app.durable.channels.iter().any(|channel| channel.id == id)
    }

    /// Deleting a channel never silently takes an editor's unsaved or
    /// in-flight work with it: every route asks first or refuses, and the
    /// confirmation names the file and offers Save before anything closes.
    #[test]
    fn unsaved_or_saving_editors_block_silent_channel_deletion() {
        use crate::repository_widget::RepositoryRuntime;
        for runtime in [
            RepositoryRuntime::dirty_fixture as fn(CardId) -> RepositoryRuntime,
            |card| RepositoryRuntime::post_persist_failure_fixture(card).0,
        ] {
            // With no conversation left in it, the pointer Delete used to
            // delete the channel at once.
            let (mut app, doomed, conversation, editor) = channel_with_an_editor(runtime);
            Arc::make_mut(&mut app.durable)
                .channels
                .iter_mut()
                .find(|channel| channel.id == doomed)
                .unwrap()
                .cards
                .retain(|card| *card != conversation);
            drop(app.handle_channel_ui(Action::Delete(doomed)));
            let dialog = app.channel_ui.dialog.as_ref().expect("deletion asks first");
            assert_eq!(dialog.consent.conversations, 0);
            assert_eq!(dialog.consent.unsaved, vec![(editor, "notes.txt".into())]);
            assert_eq!(
                unsaved_editors_notice(&dialog.consent.unsaved).as_deref(),
                Some(
                    "\"notes.txt\" has unsaved changes. You can save or discard them before the channel is deleted."
                )
            );

            // Confirming hands the editor to its own Save / Discard / Cancel
            // question; the channel and the editor are both still there.
            drop(app.handle_channel_ui(Action::ConfirmDelete));
            assert!(channel_exists(&app, doomed));
            assert!(app.dirty_repository_close.as_ref().is_some_and(|close| {
                close.card == editor && close.intent == DirtyCloseIntent::Channel(doomed)
            }));
            assert!(app.editor_holds_unsaved_work(editor));

            // Routes with no dialog of their own, such as a group deletion or
            // a retry, refuse rather than discard the editor.
            drop(app.delete_channel(doomed));
            assert!(channel_exists(&app, doomed) && !app.channel_ui.is_deleting(doomed));
            assert!(app.editor_holds_unsaved_work(editor));
            drop(app.handle_channel_ui(Action::RetryDelete(doomed)));
            assert!(channel_exists(&app, doomed) && app.editor_holds_unsaved_work(editor));
            assert!(
                app.channel_ui.failed.contains_key(&doomed),
                "a deletion already under way reports the refusal on its row"
            );
        }
    }

    /// An editor that picks up unsaved work while the channel's sessions
    /// close stops the deletion at its completion.
    #[test]
    fn unsaved_work_arriving_while_sessions_close_fails_the_deletion() {
        use crate::repository_widget::RepositoryRuntime;
        let (mut app, doomed, _conversation, editor) = channel_with_an_editor(|card| {
            RepositoryRuntime::open_fixture(card, "notes.txt", "saved\n").0
        });
        assert!(!app.editor_holds_unsaved_work(editor));
        drop(app.handle_channel_ui(Action::Delete(doomed)));
        drop(app.handle_channel_ui(Action::ConfirmDelete));
        assert!(app.channel_ui.is_deleting(doomed));

        app.cards.insert(
            editor,
            CardRuntime::Repository(Box::new(RepositoryRuntime::dirty_fixture(editor))),
        );
        drop(app.handle_channel_ui(Action::SessionsClosed(
            doomed,
            app.channel_ui.close_operation(doomed),
            Ok(()),
        )));
        assert!(channel_exists(&app, doomed));
        assert!(app.channel_ui.failed.contains_key(&doomed));
        assert!(app.editor_holds_unsaved_work(editor));
    }

    /// Each named editor is asked in turn; cancelling any of them keeps the
    /// channel, even after another was discarded.
    #[test]
    fn every_unsaved_editor_is_resolved_before_the_channel_goes() {
        use crate::repository_widget::RepositoryRuntime;
        let (mut app, doomed, _conversation, first) =
            channel_with_an_editor(RepositoryRuntime::dirty_fixture);
        let second = CardId::new();
        let durable = Arc::make_mut(&mut app.durable);
        let mut state = durable
            .cards
            .iter()
            .find(|card| card.id == first)
            .unwrap()
            .clone();
        state.id = second;
        durable.cards.push(state);
        durable
            .channels
            .iter_mut()
            .find(|channel| channel.id == doomed)
            .unwrap()
            .cards
            .push(second);
        app.cards.insert(
            second,
            CardRuntime::Repository(Box::new(RepositoryRuntime::dirty_fixture(second))),
        );

        drop(app.handle_channel_ui(Action::Delete(doomed)));
        assert_eq!(
            app.channel_ui
                .dialog
                .as_ref()
                .unwrap()
                .consent
                .unsaved
                .len(),
            2
        );
        drop(app.handle_channel_ui(Action::ConfirmDelete));
        assert!(
            app.dirty_repository_close
                .as_ref()
                .is_some_and(|close| close.card == first)
        );
        drop(app.update(Message::DiscardAndCloseRepository(first)));
        assert!(
            app.dirty_repository_close
                .as_ref()
                .is_some_and(|close| close.card == second)
        );
        assert!(!app.channel_ui.is_deleting(doomed));
        drop(app.update(Message::CancelRepositoryClose));
        assert!(app.channel_ui.resolving.is_none());
        assert!(channel_exists(&app, doomed) && !app.channel_ui.is_deleting(doomed));
        assert!(app.editor_holds_unsaved_work(second));
    }

    #[test]
    fn discarding_the_named_editor_completes_the_confirmed_deletion_and_cancel_keeps_it() {
        use crate::repository_widget::RepositoryRuntime;
        for discard in [true, false] {
            let (mut app, doomed, _conversation, editor) =
                channel_with_an_editor(RepositoryRuntime::dirty_fixture);
            drop(app.handle_channel_ui(Action::Delete(doomed)));
            drop(app.handle_channel_ui(Action::ConfirmDelete));
            if discard {
                drop(app.update(Message::DiscardAndCloseRepository(editor)));
                assert!(app.dirty_repository_close.is_none());
                assert!(
                    app.channel_ui.is_deleting(doomed),
                    "the deletion the user confirmed goes ahead"
                );
            } else {
                drop(app.update(Message::CancelRepositoryClose));
                assert!(app.channel_ui.resolving.is_none());
                assert!(!app.channel_ui.is_deleting(doomed));
                assert!(channel_exists(&app, doomed) && app.editor_holds_unsaved_work(editor));
            }
        }
    }

    /// The confirmation's account of the channel is fixed when it opens. A
    /// pane that arrives or changes source meanwhile is not deleted on the
    /// strength of a dialog that never showed it.
    #[test]
    fn deletion_consent_is_fixed_when_the_confirmation_opens() {
        let (mut app, doomed, _survivor, conversation) = channel_with_a_conversation();
        drop(app.handle_channel_ui(Action::Delete(doomed)));
        let shown = app.channel_ui.dialog.as_ref().unwrap().consent.clone();
        assert_eq!(shown.conversations, 1);

        // Another window adds a second conversation while the dialog is open.
        let arrival = CardId::new();
        let durable = Arc::make_mut(&mut app.durable);
        let mut chat = durable.cards[0].clone();
        chat.id = arrival;
        chat.kind = CardKind::Chat(chat_session());
        durable.cards.push(chat);
        durable
            .channels
            .iter_mut()
            .find(|channel| channel.id == doomed)
            .unwrap()
            .cards
            .push(arrival);
        // The open dialog still reports what it was opened with.
        assert_eq!(app.channel_ui.dialog.as_ref().unwrap().consent, shown);

        drop(app.handle_channel_ui(Action::ConfirmDelete));
        assert!(!app.channel_ui.is_deleting(doomed), "nothing is closed");
        let refreshed = app.channel_ui.dialog.as_ref().expect("asked again");
        assert!(refreshed.changed);
        assert_eq!(refreshed.consent.conversations, 2);
        assert!(
            refreshed
                .consent
                .panes
                .iter()
                .any(|(card, _)| *card == arrival)
        );

        // A pane whose session is replaced is a change too.
        let refreshed = refreshed.consent.clone();
        let durable = Arc::make_mut(&mut app.durable);
        let state = durable
            .cards
            .iter_mut()
            .find(|card| card.id == conversation)
            .unwrap();
        state.kind = CardKind::Chat(chat_session());
        drop(app.handle_channel_ui(Action::ConfirmDelete));
        assert!(!app.channel_ui.is_deleting(doomed));
        assert_ne!(app.channel_ui.dialog.as_ref().unwrap().consent, refreshed);

        // Confirming what is now shown deletes it.
        drop(app.handle_channel_ui(Action::ConfirmDelete));
        assert!(app.channel_ui.is_deleting(doomed));
    }

    /// A window whose active channel, "Investigation", holds a live
    /// conversation, beside an empty "Migration": (window, doomed, survivor,
    /// conversation).
    pub(in crate::app) fn channel_with_a_conversation() -> (Cagents, ChannelId, ChannelId, CardId) {
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
            assert!(app.navigation_save.is_some());
            // The burst's first click arms the one timer; later clicks don't
            // push it back, so a long burst is still saved within the delay.
            let generation = *armed.get_or_insert(app.navigation_save_generation);
            assert_eq!(app.navigation_save_generation, generation);
        }
        let burst = app.navigation_save_generation;
        drop(app.update(Message::NavigationSaveDue(burst)));
        assert_eq!(persists(), saved + 1, "the burst is saved once");
        assert!(app.navigation_save.is_none());

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
        assert!(app.navigation_save.is_some());
        drop(app.update(Message::NavigationSaveDue(app.navigation_save_generation)));
        assert_eq!(persists(), saved + 3);
        assert_eq!(app.durable.active_channel, Some(first));
    }

    /// The deferred save of a sidebar click writes where the window is, which
    /// the click already projected, so it projects nothing more. A save that
    /// also changes what a channel holds projects everything. The projection
    /// oracle checks both against the full projection.
    #[test]
    fn a_navigation_save_projects_only_what_it_changed() {
        use super::super::terminal_output_tests::projections;
        let (mut app, first, second, _) = channel_with_a_conversation();
        drop(app.select_channel(first));
        drop(app.update(Message::NavigationSaveDue(app.navigation_save_generation)));
        drop(app.update(Message::Sidebar(SidebarAction::OpenChannel {
            channel_id: second,
        })));
        assert!(app.navigation_save.is_some());
        let projected = projections();
        drop(app.update(Message::NavigationSaveDue(app.navigation_save_generation)));
        assert!(app.navigation_save.is_none(), "the save ran");
        assert_eq!(projections(), projected, "nothing left to project");

        // A pane closed since the click leaves its channel at the save.
        drop(app.update(Message::Sidebar(SidebarAction::OpenChannel {
            channel_id: first,
        })));
        let closed = app
            .durable
            .channels
            .iter()
            .find(|channel| channel.id == first)
            .unwrap()
            .cards[0];
        app.card_state_mut(closed).unwrap().existence =
            super::super::project_ui::removed_existence("closed by the test");
        let projected = projections();
        drop(app.update(Message::NavigationSaveDue(app.navigation_save_generation)));
        assert!(
            !app.durable
                .channels
                .iter()
                .any(|channel| channel.cards.contains(&closed))
        );
        assert_eq!(projections(), projected + 1, "the channel's panes changed");
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
pub(in crate::app) mod close_tests {
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

    pub(in crate::app) fn agent(session: &crate::chat::Session) -> Agent {
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

    fn proof() -> crate::model::HostSessionProof {
        crate::model::HostSessionProof {
            host_session_id: crate::model::HostSessionId::new("session"),
            boot_id: crate::model::BootId::new("boot"),
            host_incarnation: crate::model::HostIncarnation::new("incarnation"),
        }
    }

    fn captured_source() -> crate::chat::Session {
        let mut source = conversation(crate::chat::NativeContinuity::ReconnectOnly);
        source.launch.session_id = Some(uuid::Uuid::new_v4().to_string());
        source.proof = Some(proof());
        source
    }

    fn listed(sessions: Vec<crate::chat::Session>) -> crate::terminal::ipc::ServerResponse {
        crate::terminal::ipc::ServerResponse::ChatListed { sessions }
    }

    /// Close reads only the agent rows, not the whole workspace model.
    fn read_agents() -> crate::terminal::ipc::ClientRequest {
        crate::terminal::ipc::ClientRequest::ReadWorkspaceMetadata {
            query: crate::workspace_model::metadata::Query::Agents,
        }
    }

    fn model(agents: Vec<Agent>) -> crate::terminal::ipc::ServerResponse {
        crate::terminal::ipc::ServerResponse::WorkspaceMetadata(
            crate::workspace_model::metadata::Reply::Agents {
                identity: uuid::Uuid::nil(),
                agents,
            },
        )
    }

    /// Actual control framing checks the entire request sequence, including
    /// absence of replacement attach/close or ID-derived terminal cleanup.
    fn captured_close(
        source: &crate::chat::Session,
        steps: Vec<(
            crate::terminal::ipc::ClientRequest,
            crate::terminal::ipc::ServerResponse,
        )>,
    ) -> Result<(), String> {
        use crate::terminal::ipc::{
            ClientEnvelope, ClientRequest, PROTOCOL_VERSION, ServerEnvelope, ServerMessage,
            ServerResponse, read_frame, write_frame,
        };
        use std::os::unix::net::UnixStream;
        let (stream, mut peer) = UnixStream::pair().unwrap();
        peer.set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        peer.set_write_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let captured = source.clone();
        let server = std::thread::spawn(move || {
            let connect: ClientEnvelope = read_frame(&mut peer).unwrap().unwrap();
            assert!(matches!(connect.request, ClientRequest::Connect { .. }));
            let mut respond = |request_id, response| {
                write_frame(
                    &mut peer,
                    &ServerEnvelope {
                        version: PROTOCOL_VERSION,
                        request_id: Some(request_id),
                        proof: proof(),
                        message: ServerMessage::Response { response },
                    },
                )
                .unwrap();
            };
            respond(
                connect.request_id,
                ServerResponse::Connected {
                    proof: proof(),
                    resumed: Vec::new(),
                },
            );
            let mut terminal_inventory_read = false;
            for (expected, response) in steps {
                let request = loop {
                    let request: ClientEnvelope = read_frame(&mut peer).unwrap().unwrap();
                    assert_eq!(request.proof, Some(proof()));
                    // Ordinary Close asks for terminal inventory too. Permit
                    // that ordering so the old ID-substitution policy reaches
                    // the ownership assertion, not a fixture protocol mismatch.
                    if request.request == ClientRequest::List {
                        assert_eq!(expected, ClientRequest::ChatList);
                        assert!(
                            !terminal_inventory_read,
                            "terminal inventory may be read only once"
                        );
                        terminal_inventory_read = true;
                        write_frame(
                            &mut peer,
                            &ServerEnvelope {
                                version: PROTOCOL_VERSION,
                                request_id: Some(request.request_id),
                                proof: proof(),
                                message: ServerMessage::Response {
                                    response: ServerResponse::Listed {
                                        terminals: Vec::new(),
                                    },
                                },
                            },
                        )
                        .unwrap();
                        continue;
                    }
                    break request;
                };
                assert_eq!(request.request, expected);
                write_frame(
                    &mut peer,
                    &ServerEnvelope {
                        version: PROTOCOL_VERSION,
                        request_id: Some(request.request_id),
                        proof: proof(),
                        message: ServerMessage::Response { response },
                    },
                )
                .unwrap();
            }
            if let Some(request) = read_frame::<ClientEnvelope>(&mut peer).unwrap() {
                if let ClientRequest::ChatAttach { session } = &request.request {
                    assert_eq!(
                        session, &captured,
                        "Fresh must never attach the replacement occupying the captured native ID"
                    );
                }
                panic!(
                    "unexpected cleanup after the captured source: {:?}",
                    request.request
                );
            }
        });
        let mut client = crate::control::ControlClient::from_stream(stream).unwrap();
        let result = close_captured_chat_with(&mut client, source);
        drop(client);
        server.join().unwrap();
        result
    }

    fn paired_source() -> crate::chat::Session {
        let mut source = captured_source();
        let host = proof();
        source.terminal = Some(Box::new(crate::model::TerminalSession {
            id: source.id,
            launch: source.launch.recipe.clone(),
            owned_codex_listener: None,
            recovery_origin: None,
            predecessor: None,
            transcript_path: None,
            resume_hint: None,
            continuity: crate::model::TerminalContinuity::Live {
                host_session_id: host.host_session_id,
                boot_id: host.boot_id,
                host_incarnation: host.host_incarnation,
            },
        }));
        source
    }

    #[test]
    fn fresh_close_requires_receipt_for_absent_retained_owner_or_pair() {
        use crate::terminal::ipc::{ClientRequest, ServerResponse};
        let mut retained = captured_source();
        retained.native_continuity = crate::chat::NativeContinuity::Durable {
            provider_version: "fixture".into(),
        };
        for source in [retained, paired_source()] {
            assert!(no_daemon_captured_close_result(&source).is_err());
            assert!(
                captured_close(
                    &source,
                    vec![
                        (ClientRequest::ChatList, listed(vec![])),
                        (read_agents(), model(vec![])),
                        (
                            ClientRequest::ChatCloseRetained {
                                session: source.clone()
                            },
                            ServerResponse::Accepted
                        ),
                    ]
                )
                .is_ok()
            );
            assert!(
                captured_close(
                    &source,
                    vec![
                        (ClientRequest::ChatList, listed(vec![])),
                        (read_agents(), model(vec![])),
                        (
                            ClientRequest::ChatCloseRetained {
                                session: source.clone()
                            },
                            ServerResponse::Detached
                        ),
                    ]
                )
                .is_err()
            );
        }
        assert!(no_daemon_captured_close_result(&captured_source()).is_ok());
    }

    #[test]
    fn fresh_close_refuses_same_id_replacement_before_attachment() {
        use crate::terminal::ipc::ClientRequest;
        let source = captured_source();
        for change in 0..5 {
            let mut replacement = source.clone();
            match change {
                0 => replacement.launch.session_id = Some(uuid::Uuid::new_v4().to_string()),
                1 => replacement.launch.recipe.arguments.push("--changed".into()),
                2 => {
                    replacement
                        .launch
                        .recipe
                        .environment
                        .insert("ACCOUNT".into(), "other".into());
                }
                3 => {
                    replacement.proof.as_mut().unwrap().host_incarnation =
                        crate::model::HostIncarnation::new("new-owner")
                }
                _ => {
                    replacement.native_continuity =
                        crate::chat::NativeContinuity::RetainedClaudeActor { protocol: 2 }
                }
            }
            assert!(
                captured_close(
                    &source,
                    vec![(ClientRequest::ChatList, listed(vec![replacement]))]
                )
                .is_err(),
                "replacement variant {change}"
            );
        }
    }

    #[test]
    fn fresh_close_checks_attached_descriptor_before_close() {
        use crate::terminal::ipc::{ClientRequest, ServerResponse};
        let source = captured_source();
        let mut replacement = source.clone();
        replacement.launch.session_id = Some(uuid::Uuid::new_v4().to_string());
        assert!(
            captured_close(
                &source,
                vec![
                    (ClientRequest::ChatList, listed(vec![source.clone()])),
                    (read_agents(), model(vec![])),
                    (
                        ClientRequest::ChatAttach {
                            session: source.clone()
                        },
                        ServerResponse::ChatAttached {
                            session: replacement,
                            snapshot: Default::default()
                        }
                    ),
                    (
                        ClientRequest::ChatDetach { chat_id: source.id },
                        ServerResponse::Detached
                    ),
                ]
            )
            .is_err()
        );
    }

    /// Closing an open agent's conversation retires the agent and removes its
    /// pane, so Fresh would start in a card that no longer exists. It refuses
    /// before closing anything, and again if an agent claims it mid-close.
    #[test]
    fn fresh_close_refuses_a_conversation_an_open_agent_runs_as() {
        use crate::terminal::ipc::{ClientRequest, ServerResponse};
        let source = captured_source();
        for live in [false, true] {
            assert_eq!(
                captured_close(
                    &source,
                    vec![
                        (
                            ClientRequest::ChatList,
                            listed(if live { vec![source.clone()] } else { vec![] })
                        ),
                        (read_agents(), model(vec![agent(&source)])),
                    ]
                ),
                Err(agent_owned_conversation())
            );
        }
        assert_eq!(
            captured_close(
                &source,
                vec![
                    (ClientRequest::ChatList, listed(vec![source.clone()])),
                    (read_agents(), model(vec![])),
                    (
                        ClientRequest::ChatAttach {
                            session: source.clone()
                        },
                        ServerResponse::ChatAttached {
                            session: source.clone(),
                            snapshot: Default::default()
                        }
                    ),
                    (
                        ClientRequest::ChatClose { chat_id: source.id },
                        ServerResponse::ChatClosed {
                            chat_id: source.id,
                            snapshot: Default::default()
                        }
                    ),
                    (read_agents(), model(vec![agent(&source)])),
                ]
            ),
            Err(agent_owned_conversation())
        );
    }

    #[test]
    fn fresh_close_never_follows_agent_replacement_before_or_after_close() {
        use crate::terminal::ipc::{ClientRequest, ServerResponse};
        let source = captured_source();
        let mut replacement = source.clone();
        replacement.launch.session_id = Some(uuid::Uuid::new_v4().to_string());
        for live in [false, true] {
            assert!(
                captured_close(
                    &source,
                    vec![
                        (
                            ClientRequest::ChatList,
                            listed(if live { vec![source.clone()] } else { vec![] })
                        ),
                        (read_agents(), model(vec![agent(&replacement)])),
                    ]
                )
                .is_err()
            );
        }
        assert!(
            captured_close(
                &source,
                vec![
                    (ClientRequest::ChatList, listed(vec![source.clone()])),
                    (read_agents(), model(vec![])),
                    (
                        ClientRequest::ChatAttach {
                            session: source.clone()
                        },
                        ServerResponse::ChatAttached {
                            session: source.clone(),
                            snapshot: Default::default()
                        }
                    ),
                    (
                        ClientRequest::ChatClose { chat_id: source.id },
                        ServerResponse::ChatClosed {
                            chat_id: source.id,
                            snapshot: Default::default()
                        }
                    ),
                    (read_agents(), model(vec![agent(&replacement)])),
                ]
            )
            .is_err()
        );
    }

    #[test]
    fn fresh_close_of_an_absent_unretained_conversation_asks_nothing_more() {
        use crate::terminal::ipc::ClientRequest;
        let source = captured_source();
        assert!(
            captured_close(
                &source,
                vec![
                    (ClientRequest::ChatList, listed(vec![])),
                    (read_agents(), model(vec![])),
                ]
            )
            .is_ok()
        );
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
