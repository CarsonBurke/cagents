//! Whether a window may retire a channel it saw empty.

use rusqlite::{OptionalExtension, Transaction};
use uuid::Uuid;

use crate::{
    model::{CardId, ChannelId},
    workspace_model::{ChannelIncarnation, views::Scope},
};

#[cfg(test)]
mod tests;

/// Whether `home` is still the channel it names, its view has held no pane
/// since `expected_view_revision`, and no open agent placed there still awaits
/// a pane. A pane published in between, by any window, moves the revision and
/// keeps the channel. A changed incarnation is an error: the window's
/// candidate names a channel that no longer exists.
pub(super) fn may_retire_in(
    tx: &Transaction<'_>,
    home: &ChannelIncarnation,
    expected_view_revision: u64,
) -> Result<bool, String> {
    let identity: String = tx
        .query_row("SELECT identity FROM metadata WHERE key=0", [], |row| {
            row.get(0)
        })
        .map_err(|error| error.to_string())?;
    let identity: Uuid = identity
        .parse()
        .map_err(|_| "The registry identity is unreadable")?;
    if identity != home.registry {
        return Err("The channel belongs to another registry".into());
    }
    let ordinal: Option<u32> = tx
        .query_row(
            "SELECT ordinal FROM channels WHERE id=?1",
            [home.channel.to_string()],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| error.to_string())?;
    match ordinal {
        None => return Err("The channel no longer exists".into()),
        Some(ordinal) if ordinal != home.creation_ordinal => {
            return Err("The channel incarnation changed".into());
        }
        Some(_) => {}
    }
    let view = super::views::read(tx, &Scope::Channel(home.channel))?;
    if view.revision != expected_view_revision || !view.panes.is_empty() {
        return Ok(false);
    }
    Ok(!holds_detached_agent_in(tx, home.channel, &view.retired)?)
}

/// An open agent placed in the channel whose pane was never closed there is a
/// session waiting for a window to show it; retiring the channel would hide it.
fn holds_detached_agent_in(
    tx: &Transaction<'_>,
    channel: ChannelId,
    closed_panes: &[CardId],
) -> Result<bool, String> {
    let mut statement = tx
        .prepare_cached("SELECT record FROM agents WHERE closed=0 AND instr(record, ?1) > 0")
        .map_err(|error| error.to_string())?;
    let records = statement
        .query_map([channel.to_string()], |row| row.get::<_, String>(0))
        .map_err(|error| error.to_string())?;
    for record in records {
        let record = record.map_err(|error| error.to_string())?;
        // An unreadable record is not proof of absence: keep the channel.
        let Ok(agent) = serde_json::from_str::<crate::agent_execution::Agent>(&record) else {
            return Ok(true);
        };
        if agent.awaits_pane_in(channel, closed_panes) {
            return Ok(true);
        }
    }
    Ok(false)
}
