//! Moves one pane into a channel of its project for a person or an agent.
//!
//! The shared model owns placement, and every window applies a committed
//! transfer as it applies another window's: the card keeps its conversation,
//! terminal and unsent draft. So this commits the same `Transfer` a window's
//! own move sends, after the authority checks an agent's connection needs,
//! and makes the destination channel first when the caller names a new one.
use super::{super::agent_execution::terminals, edit_delta, edit_then, snapshot, unavailable};
use crate::{
    host::state::{ClientId, SharedHost},
    model::{CardId, CardKind, ChannelId},
    terminal_ipc::{ProtocolError, ProtocolErrorCode},
    workspace_model::{
        ChannelIncarnation, ModelChannel, ModelEdit, PaneDestination, PaneMove, WorkspaceModel,
        views::{Change, Pane, Scope, View},
    },
};
use std::path::Path;

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
        let (view, found) = locate(&model, mover, pane)?;
        // Authorized against this revision: a pane whose session changes
        // before the transfer commits changes the revision and is read again.
        terminals::authorize_move(host, mover, found)?;
        let repository = project(&model, &view.scope)
            .ok_or_else(|| refused("The pane's project is no longer open, so it can't move"))?;
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
        if view.scope == to {
            return Ok(PaneMove {
                pane: found.clone(),
                from: to,
                channel: channel.clone(),
                created: false,
                sequence: model.sequence,
            });
        }
        let id = found.id;
        let from = view.scope.clone();
        let edit = ModelEdit::ChangeView {
            scope: from.clone(),
            expected_revision: view.revision,
            operation,
            change: Change::Transfer {
                id,
                destination: to.clone(),
                before: None,
            },
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
        let pane = after
            .and_then(|model| model.views.iter().find(|view| view.scope == to))
            .and_then(|view| view.panes.iter().find(|pane| pane.id == id))
            .unwrap_or(found)
            .clone();
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

/// The pane to move and the view holding it: the named pane, or the pane
/// showing the agent's own live session.
fn locate<'a>(
    model: &'a WorkspaceModel,
    mover: &terminals::Mover,
    pane: Option<CardId>,
) -> Result<(&'a View, &'a Pane), ProtocolError> {
    let panes = || {
        model
            .views
            .iter()
            .flat_map(|view| view.panes.iter().map(move |pane| (view, pane)))
    };
    match (pane, mover) {
        (Some(id), _) => panes().find(|(_, pane)| pane.id == id).ok_or_else(|| {
            refused(format!(
                "No saved pane {id}. A pane that only one window shows yet can be moved from that window"
            ))
        }),
        (None, terminals::Mover::Person) => Err(refused(
            "Name the pane to move: `cagents channel move PANE --to CHANNEL`",
        )),
        (None, terminals::Mover::Agent { session, .. }) => {
            let mut own = panes().filter(|(_, pane)| shows(pane, *session));
            let found = own.next().ok_or_else(|| {
                refused(
                    "Your session has no saved pane in a channel yet, so it can't move. Ask the person to move it from the window",
                )
            })?;
            if own.next().is_some() {
                return Err(refused(
                    "Your session shows in more than one pane; name the pane to move by its ID",
                ));
            }
            Ok(found)
        }
    }
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
