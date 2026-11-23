//! Moves one pane into a channel of its project for a person or an agent.
//!
//! The shared model owns placement, and every window applies a committed
//! transfer as it applies another window's: the card keeps its conversation,
//! terminal and unsent draft. So this commits the same `Transfer` a window's
//! own move sends, after the authority checks an agent's connection needs,
//! and makes the destination channel first when the caller names a new one.
//!
//! A live conversation can outlive every pane that showed it, as when its
//! channel is deleted or a window's stop is refused. Naming its session
//! gives it a new pane in the destination instead: the same conversation,
//! with no provider started or replayed.
use super::{super::agent_execution::terminals, edit_delta, edit_then, snapshot, unavailable};
use crate::{
    host::state::{ClientId, SharedHost},
    model::{CardId, CardKind, ChannelId, TerminalId},
    terminal_ipc::{ProtocolError, ProtocolErrorCode},
    workspace_model::{
        ChannelIncarnation, ModelChannel, ModelEdit, PaneDestination, PaneMove, WorkspaceModel,
        views::{Change, Pane, Scope, View},
    },
};
use std::path::{Path, PathBuf};

/// Conflicting edits a move re-reads and re-authorizes before it gives up.
const ATTEMPTS: usize = 3;

#[cfg(test)]
thread_local! {
    static BEFORE_TRANSFER: std::cell::RefCell<Option<Box<dyn FnMut()>>> = const { std::cell::RefCell::new(None) };
}

/// Runs `hook` on this thread before each transfer a move commits, as a
/// window's edit arriving between the read and the transfer would; `None`
/// removes it.
#[cfg(test)]
pub(in crate::host) fn before_transfer(hook: Option<Box<dyn FnMut()>>) {
    BEFORE_TRANSFER.with(|slot| *slot.borrow_mut() = hook);
}

/// A channel this request made, as it was made: only that incarnation, still
/// empty at that revision, is removed when the move fails.
struct Made {
    home: ChannelIncarnation,
    view_revision: u64,
}

pub(in crate::host) fn move_pane(
    host: &SharedHost,
    origin: ClientId,
    pane: Option<CardId>,
    destination: PaneDestination,
    operation: uuid::Uuid,
) -> Result<PaneMove, ProtocolError> {
    let mover = terminals::mover(host, origin)?;
    let mut made = None;
    let moved = attempt(
        host,
        origin,
        &mover,
        pane,
        &destination,
        operation,
        &mut made,
    );
    if moved.is_err()
        && let Some(Made {
            home,
            view_revision,
        }) = made
    {
        // The move that would have filled the channel failed before it
        // committed. Retire the channel only while it is still the empty one
        // this request made: whatever a window has put there since stays.
        let _ = edit_delta(
            host,
            origin,
            ModelEdit::RetireEmptyChannel {
                home,
                expected_view_revision: view_revision,
            },
        );
    }
    moved
}

fn attempt(
    host: &SharedHost,
    origin: ClientId,
    mover: &terminals::Mover,
    pane: Option<CardId>,
    destination: &PaneDestination,
    operation: uuid::Uuid,
    made: &mut Option<Made>,
) -> Result<PaneMove, ProtocolError> {
    let mut conflicts = 0;
    loop {
        let model = snapshot(host, None)?;
        let found = locate(host, &model, mover, pane)?;
        // Authorized against this revision: a pane whose session changes
        // before the transfer commits changes the revision and is read again.
        terminals::authorize_move(host, mover, found.pane())?;
        let repository = match &found {
            Found::Saved(view, _) => project(&model, &view.scope)
                .ok_or_else(|| refused("The pane's project is no longer open, so it can't move"))?,
            Found::Unshown { repository, .. } => repository.as_path(),
        };
        let (target, suggest_new) = match destination {
            PaneDestination::Channel(id) => (*id, true),
            PaneDestination::NewChannel { id, .. } => (*id, false),
        };
        let channel = match (model.channel(target), destination) {
            (Some(channel), _) => channel,
            (None, PaneDestination::NewChannel { id, title }) if made.is_none() => {
                *made = Some(make_channel(host, origin, *id, title, repository)?);
                // Read again, so the new channel is checked as any other is.
                continue;
            }
            (None, PaneDestination::NewChannel { .. }) => {
                return Err(refused(
                    "The channel made for this pane closed before the pane moved; run the move again",
                ));
            }
            (None, PaneDestination::Channel(id)) => {
                return Err(refused(format!(
                    "No open channel {id}. List this project's channels with `cagents channel list --repository {}`, or make one with `--new TITLE`",
                    repository.display()
                )));
            }
        };
        same_project(channel, repository, suggest_new)?;
        let to = Scope::Channel(target);
        let opened = matches!(found, Found::Unshown { .. });
        let (id, from, edit) = match found {
            Found::Saved(view, found) if view.scope == to => {
                return Ok(PaneMove {
                    pane: found.clone(),
                    from: to,
                    channel: channel.clone(),
                    created: false,
                    sequence: model.sequence,
                });
            }
            Found::Saved(view, found) => (
                found.id,
                view.scope.clone(),
                ModelEdit::ChangeView {
                    scope: view.scope.clone(),
                    expected_revision: view.revision,
                    operation,
                    change: Change::Transfer {
                        id: found.id,
                        destination: to.clone(),
                        before: None,
                    },
                },
            ),
            Found::Unshown { pane, home, .. } => (
                pane.id,
                home,
                ModelEdit::ChangeView {
                    scope: to.clone(),
                    expected_revision: model
                        .views
                        .iter()
                        .find(|view| view.scope == to)
                        .map_or(0, |view| view.revision),
                    operation,
                    change: Change::Insert {
                        pane: *pane,
                        before: None,
                    },
                },
            ),
        };
        #[cfg(test)]
        {
            let hook = BEFORE_TRANSFER.with(|slot| slot.borrow_mut().take());
            if let Some(mut hook) = hook {
                hook();
                BEFORE_TRANSFER.with(|slot| *slot.borrow_mut() = Some(hook));
            }
        }
        // Read with the writer still held, so the reply describes this edit
        // and no later one. Once the transfer commits the move has happened:
        // nothing after it may fail the request or remove the channel.
        let (receipt, after) = match edit_then(host, origin, edit, |registry| {
            Ok(registry.model().ok())
        }) {
            Err(error)
                if error.code == ProtocolErrorCode::ViewConflict && conflicts + 1 < ATTEMPTS =>
            {
                conflicts += 1;
                continue;
            }
            result => result?,
        };
        let after = after.as_ref();
        let placed = after
            .and_then(|model| model.views.iter().find(|view| view.scope == to))
            .and_then(|view| view.panes.iter().find(|pane| pane.id == id));
        let pane = match placed {
            Some(pane) => pane.clone(),
            // The registry adds no second pane for a conversation a saved
            // pane already shows, so a window saved its own pane for this one
            // meanwhile. The edit's operation is spent, so moving that pane
            // takes a new request.
            None if opened => {
                return Err(refused(
                    "A window showed this session in a pane meanwhile; run the move again to move that pane",
                ));
            }
            None => {
                return Err(refused(
                    "The pane changed while it moved; run the move again",
                ));
            }
        };
        let channel = after
            .and_then(|model| model.channel(target))
            .unwrap_or(channel)
            .clone();
        return Ok(PaneMove {
            pane,
            from,
            channel,
            created: made.is_some(),
            sequence: receipt.sequence,
        });
    }
}

/// Makes the channel a `--new` move names, in the pane's project.
fn make_channel(
    host: &SharedHost,
    origin: ClientId,
    id: ChannelId,
    title: &str,
    repository: &Path,
) -> Result<Made, ProtocolError> {
    let title = crate::workspace_model::channel_title(title)
        .ok_or_else(|| refused("Name the new channel: `--new TITLE` with a visible title"))?;
    let edit = ModelEdit::CreateChannel {
        id,
        repository: repository.to_path_buf(),
        title,
        title_source: crate::model::ChannelTitleSource::Manual,
        checkout: None,
    };
    let (_, made) = edit_then(host, origin, edit, |registry| {
        let model = registry.model().map_err(unavailable)?;
        Ok(model.channel(id).map(|channel| Made {
            home: ChannelIncarnation {
                registry: model.identity,
                channel: id,
                creation_ordinal: channel.ordinal,
            },
            view_revision: model
                .views
                .iter()
                .find(|view| view.scope == Scope::Channel(id))
                .map_or(0, |view| view.revision),
        }))
    })?;
    // The registry does not make a deleted channel's id again.
    made.ok_or_else(|| refused("That channel was closed; run the move again"))
}

/// What a move places.
enum Found<'a> {
    /// A saved pane, in the view holding it.
    Saved(&'a View, &'a Pane),
    /// A live conversation no saved pane shows, with the pane it would get,
    /// the checkout it works in and that checkout's open project.
    Unshown {
        pane: Box<Pane>,
        home: Scope,
        repository: PathBuf,
    },
}

impl Found<'_> {
    fn pane(&self) -> &Pane {
        match self {
            Self::Saved(_, pane) => pane,
            Self::Unshown { pane, .. } => pane.as_ref(),
        }
    }
}

/// The pane to move: the named pane, the one pane showing the named
/// session, a new pane for a live conversation no saved pane shows, or
/// the pane showing the agent's own live session.
fn locate<'a>(
    host: &SharedHost,
    model: &'a WorkspaceModel,
    mover: &terminals::Mover,
    pane: Option<CardId>,
) -> Result<Found<'a>, ProtocolError> {
    let panes = || {
        model
            .views
            .iter()
            .flat_map(|view| view.panes.iter().map(move |pane| (view, pane)))
    };
    let only = |session: TerminalId, many: &str| {
        let mut showing = panes().filter(|(_, pane)| shows(pane, session));
        let found = showing.next();
        match showing.next() {
            Some(_) => Err(refused(many)),
            None => Ok(found),
        }
    };
    match (pane, mover) {
        (Some(id), _) => {
            if let Some((view, pane)) = panes().find(|(_, pane)| pane.id == id) {
                return Ok(Found::Saved(view, pane));
            }
            // Not a pane: perhaps a session, saved in one pane or in none.
            let session = TerminalId::from_uuid(id.as_uuid());
            if let Some((view, pane)) = only(
                session,
                "That session shows in more than one pane; name the pane to move by its ID",
            )? {
                return Ok(Found::Saved(view, pane));
            }
            unshown(host, model, session)?.ok_or_else(|| {
                refused(format!(
                    "No saved pane or live session {id}. A pane that only one window shows yet can be moved from that window"
                ))
            })
        }
        (None, terminals::Mover::Person) => Err(refused(
            "Name the pane to move: `cagents channel move PANE --to CHANNEL`",
        )),
        (None, terminals::Mover::Agent { session, .. }) => only(
            *session,
            "Your session shows in more than one pane; name the pane to move by its ID",
        )?
        .map(|(view, pane)| Found::Saved(view, pane))
        .ok_or_else(|| {
            refused(
                "Your session has no saved pane in a channel yet, so it can't move. Ask the person to move it from the window",
            )
        }),
    }
}

/// A new pane for `session`, a live conversation no saved pane shows, or
/// `None` when the daemon hosts no such conversation. Refused while a
/// window still shows it in a pane that window has not saved, which that
/// window moves itself, and for an agent's conversation, whose pane a
/// window opens in the agent's channel.
fn unshown(
    host: &SharedHost,
    model: &WorkspaceModel,
    session: TerminalId,
) -> Result<Option<Found<'static>>, ProtocolError> {
    if let Some(agent) = model.agents.iter().find(|agent| {
        !agent.closed
            && agent
                .session
                .as_ref()
                .is_some_and(|hosted| hosted.id() == session)
    }) {
        let name = agent.name.clone().unwrap_or_else(|| agent.id.to_string());
        return Err(refused(format!(
            "Session {session} is agent {name}'s, and a window opens its pane in the agent's channel; move that pane once it shows"
        )));
    }
    let (source, title) = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        let Some(record) = state.chats.get(&session) else {
            return Ok(None);
        };
        if record.handle.is_completed() {
            return Err(refused(format!(
                "Session {session} has ended; resume its conversation from a window instead"
            )));
        }
        if !record.subscribers.is_empty() {
            return Err(refused(format!(
                "Another client is showing session {session} right now, such as a window in a pane it has not saved or the phone app. Move it from that window, or close it there and run the move again"
            )));
        }
        (record.current_session(), record.handle.title())
    };
    let directory = &source.launch.recipe.working_directory;
    let placement = crate::project_location::place(directory, model);
    let Some(repository) = placement.project else {
        return Err(refused(format!(
            "Session {session} works in {}, which no open project contains. Open its project with `cagents project open PATH` first",
            directory.display()
        )));
    };
    let checkout = placement.worktree.unwrap_or_else(|| repository.clone());
    let pane = Box::new(Pane {
        id: CardId::new(),
        // The conversation's own title where it has one, as its old tab had.
        title: title
            .filter(|title| !title.trim().is_empty())
            .unwrap_or_else(|| source.launch.provider.chat_title().into()),
        user_title: None,
        pending_terminal_start: false,
        width_fraction: crate::settings::DEFAULT_NATIVE_PANE_FRACTION,
        // The checkout the conversation works in, as an agent's pane names
        // the directory it was started in.
        scope: Some(checkout.clone()),
        kind: CardKind::Chat(source),
    });
    Ok(Some(Found::Unshown {
        pane,
        home: Scope::Worktree {
            repository: repository.clone(),
            path: checkout,
        },
        repository,
    }))
}

fn shows(pane: &Pane, session: crate::model::TerminalId) -> bool {
    match &pane.kind {
        CardKind::Chat(chat) => chat.id == session,
        CardKind::Terminal(terminal) => terminal.id == session,
        _ => false,
    }
}

fn project<'a>(model: &'a WorkspaceModel, scope: &'a Scope) -> Option<&'a Path> {
    match scope {
        Scope::Channel(id) => model
            .channel(*id)
            .map(|channel| channel.repository.as_path()),
        Scope::Worktree { repository, .. } => Some(repository),
    }
}

fn same_project(
    channel: &ModelChannel,
    pane: &Path,
    suggest_new: bool,
) -> Result<(), ProtocolError> {
    if channel.repository == pane {
        return Ok(());
    }
    let title = if channel.title.is_empty() {
        "New channel"
    } else {
        &channel.title
    };
    let instead = if suggest_new {
        "; `--new TITLE` makes a channel there"
    } else {
        ""
    };
    Err(refused(format!(
        "Channel \"{title}\" is in {}, and this pane works in {}. A pane moves only between channels of its own project{instead}",
        channel.repository.display(),
        pane.display()
    )))
}

fn refused(message: impl Into<String>) -> ProtocolError {
    ProtocolError::new(ProtocolErrorCode::InvalidRequest, message, false)
}
