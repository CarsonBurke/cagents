//! Private placement generations. These are comparison values, never authority.
//! Active mappings are bounded by current panes; the singleton clock prevents
//! removed or returning card identities from reviving an old captured claim.

use crate::{
    agent_execution::HostedSession,
    model::{CardId, CardKind},
    workspace_model::views::{Pane, View},
};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use std::collections::{BTreeMap, HashSet};

pub(in crate::host::model) const TABLE_SQL: &str = "CREATE TABLE channel_work_placement_epochs(
    pane TEXT PRIMARY KEY,
    epoch INTEGER NOT NULL CHECK(epoch > 0)
) STRICT;
CREATE TABLE channel_work_placement_clock(
    id INTEGER PRIMARY KEY CHECK(id=1),
    last_epoch INTEGER NOT NULL CHECK(last_epoch>=0)
) STRICT;
INSERT INTO channel_work_placement_clock(id,last_epoch) VALUES(1,0);";

/// Called only by the owning published-schema migration, within its transaction.
/// Existing canonical panes receive a generation immediately, never on demand
/// while resolving a caller. A bad/ambiguous backfill rolls back with the DDL.
pub(in crate::host::model) fn migrate(tx: &Transaction<'_>) -> Result<(), String> {
    tx.execute_batch(TABLE_SQL)
        .map_err(|error| error.to_string())?;
    let mut seen = HashSet::new();
    let mut last_epoch = 0;
    for view in super::super::views::all(tx)? {
        for pane in view.panes {
            validate_pane(&pane)?;
            if view.retired.contains(&pane.id) || !seen.insert(pane.id) {
                return Err(
                    "Cannot assign a placement epoch to an ambiguous canonical pane".into(),
                );
            }
            let next = allocate(tx, &mut last_epoch)?;
            tx.execute(
                "INSERT INTO channel_work_placement_epochs(pane,epoch) VALUES(?1,?2)",
                params![pane.id.to_string(), next],
            )
            .map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}

fn validate_pane(pane: &Pane) -> Result<(), String> {
    if pane.id.as_uuid().is_nil() {
        return Err("Cannot assign a placement epoch to an empty pane identity".into());
    }
    pane.validate()
}

fn same_source(before: &CardKind, after: &CardKind) -> bool {
    if before == after {
        return true;
    }
    match (before, after) {
        (CardKind::Chat(before), CardKind::Chat(after)) => {
            HostedSession::Chat(before.clone()).same_process(&HostedSession::Chat(after.clone()))
        }
        (CardKind::Terminal(before), CardKind::Terminal(after)) => {
            HostedSession::Terminal(before.clone())
                .same_process(&HostedSession::Terminal(after.clone()))
        }
        _ => false,
    }
}

fn read_clock(db: &Connection) -> Result<i64, String> {
    let mut statement = db
        .prepare("SELECT id,last_epoch FROM channel_work_placement_clock LIMIT 2")
        .map_err(|error| error.to_string())?;
    let mut rows = statement.query([]).map_err(|error| error.to_string())?;
    let row = rows
        .next()
        .map_err(|error| error.to_string())?
        .ok_or("The canonical placement clock is missing")?;
    let id: i64 = row.get(0).map_err(|error| error.to_string())?;
    let last: i64 = row.get(1).map_err(|error| error.to_string())?;
    if id != 1 || last < 0 || rows.next().map_err(|error| error.to_string())?.is_some() {
        return Err("The canonical placement clock is invalid".into());
    }
    Ok(last)
}

fn allocate(db: &Connection, last: &mut i64) -> Result<i64, String> {
    let next = last
        .checked_add(1)
        .ok_or("The canonical placement clock is exhausted")?;
    let changed = db
        .execute(
            "UPDATE channel_work_placement_clock SET last_epoch=?1 WHERE id=1 AND last_epoch=?2",
            params![next, *last],
        )
        .map_err(|error| error.to_string())?;
    if changed != 1 {
        return Err("The canonical placement clock changed concurrently".into());
    }
    *last = next;
    Ok(next)
}

/// Compare actual canonical before/after views, not the client's proposed edit.
/// The caller must hold its existing write transaction and roll it back on any
/// error. Call before publishing the view, including the direct source-adoption
/// path. A transfer must remove the source mapping before inserting its target
/// in that same transaction; an insertion never adopts an already-owned row.
pub(in crate::host::model) fn observe_view_transition(
    db: &Connection,
    before: &View,
    after: &View,
) -> Result<(), String> {
    if db.is_autocommit() {
        return Err("Placement changes require the canonical view transaction".into());
    }
    let mut panes: BTreeMap<String, (Option<&Pane>, Option<&Pane>)> = BTreeMap::new();
    for (view, is_before) in [(before, true), (after, false)] {
        for pane in &view.panes {
            validate_pane(pane)?;
            if view.retired.contains(&pane.id) {
                return Err("A retired pane cannot own a placement epoch".into());
            }
            let entry = panes.entry(pane.id.to_string()).or_default();
            let slot = if is_before {
                &mut entry.0
            } else {
                &mut entry.1
            };
            if slot.replace(pane).is_some() {
                return Err("A canonical view contains duplicate pane identities".into());
            }
        }
    }
    let mut last_epoch = read_clock(db)?;
    // Validate all existing mappings before the stable-source fast path. This
    // is one bound JSON query, not one SELECT per unchanged pane on every edit.
    let ids = before
        .panes
        .iter()
        .map(|pane| pane.id.to_string())
        .collect::<Vec<_>>();
    let encoded = serde_json::to_string(&ids).map_err(|error| error.to_string())?;
    let expected = i64::try_from(ids.len()).map_err(|_| "The canonical pane set is too large")?;
    let actual: i64 = db
        .query_row(
            "SELECT count(*) FROM json_each(?1) wanted
         JOIN channel_work_placement_epochs mapped ON mapped.pane=wanted.value
         WHERE typeof(mapped.epoch)='integer' AND mapped.epoch>0 AND mapped.epoch<=?2",
            params![encoded, last_epoch],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    if actual != expected {
        return Err("A canonical pane placement epoch is missing or invalid".into());
    }
    for (_, (old, new)) in panes {
        if let (Some(old), Some(new)) = (old, new)
            && before.scope == after.scope
            && old.pending_terminal_start == new.pending_terminal_start
            && old.scope == new.scope
            && same_source(&old.kind, &new.kind)
        {
            continue;
        }
        let card = old.or(new).unwrap().id;
        match (old, new) {
            (Some(_), None) => {
                let changed = db
                    .execute(
                        "DELETE FROM channel_work_placement_epochs WHERE pane=?1",
                        [card.to_string()],
                    )
                    .map_err(|error| error.to_string())?;
                if changed != 1 {
                    return Err("The canonical pane placement disappeared".into());
                }
            }
            (Some(_), Some(_)) => {
                let next = allocate(db, &mut last_epoch)?;
                let changed = db
                    .execute(
                        "UPDATE channel_work_placement_epochs SET epoch=?2 WHERE pane=?1",
                        params![card.to_string(), next],
                    )
                    .map_err(|error| error.to_string())?;
                if changed != 1 {
                    return Err("The canonical pane placement disappeared".into());
                }
            }
            (None, Some(_)) => {
                let next = allocate(db, &mut last_epoch)?;
                let inserted = db
                    .execute(
                        "INSERT INTO channel_work_placement_epochs(pane,epoch) VALUES(?1,?2)",
                        params![card.to_string(), next],
                    )
                    .map_err(|error| error.to_string())?;
                if inserted != 1 {
                    return Err("The canonical pane placement was not inserted".into());
                }
            }
            (None, None) => unreachable!(),
        }
    }
    Ok(())
}

/// A positive durable placement epoch. It grants nothing: resolution must still
/// prove this card's unique current placement and exact authenticated source.
pub(in crate::host::model) fn token(db: &Connection, card: CardId) -> Result<u64, String> {
    let clock = read_clock(db)?;
    let value: Option<i64> = db
        .query_row(
            "SELECT epoch FROM channel_work_placement_epochs WHERE pane=?1",
            [card.to_string()],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| error.to_string())?;
    let value = value.ok_or("The canonical pane has no placement epoch")?;
    if value <= 0 || value > clock {
        return Err("The canonical pane placement epoch is invalid".into());
    }
    Ok(value as u64)
}

#[cfg(test)]
mod tests;
