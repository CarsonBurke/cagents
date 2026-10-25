//! Private placement generations. These are comparison values, never authority.
//! Keep tombstones after removal: a returning card must not revive an old claim.

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
) STRICT;";

// Old view revisions are nonnegative SQLite INTEGERs. The disjoint high bit
// prevents an old, unversioned saved Caller from accidentally matching an epoch.
const TOKEN_TAG: u64 = 1 << 63;

/// Called only by the owning published-schema migration, within its transaction.
/// Existing canonical panes receive a generation immediately, never on demand
/// while resolving a caller. A bad/ambiguous backfill rolls back with the DDL.
pub(in crate::host::model) fn migrate(tx: &Transaction<'_>) -> Result<(), String> {
    tx.execute_batch(TABLE_SQL)
        .map_err(|error| error.to_string())?;
    let mut seen = HashSet::new();
    for view in super::super::views::all(tx)? {
        for pane in view.panes {
            validate_pane(&pane)?;
            if view.retired.contains(&pane.id) || !seen.insert(pane.id) {
                return Err(
                    "Cannot assign a placement epoch to an ambiguous canonical pane".into(),
                );
            }
            tx.execute(
                "INSERT INTO channel_work_placement_epochs(pane,epoch) VALUES(?1,1)",
                [pane.id.to_string()],
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

fn epoch(db: &Connection, card: CardId) -> Result<Option<i64>, String> {
    let value: Option<i64> = db
        .query_row(
            "SELECT epoch FROM channel_work_placement_epochs WHERE pane=?1",
            [card.to_string()],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| error.to_string())?;
    if value.is_some_and(|value| value <= 0) {
        return Err("The canonical pane placement epoch is invalid".into());
    }
    Ok(value)
}

/// Compare actual canonical before/after views, not the client's proposed edit.
/// The caller must hold its existing write transaction and roll it back on any
/// error. Call before publishing the view, including the direct source-adoption
/// path. A transfer can rotate twice in that same transaction; only non-reuse,
/// not an exact increment count, is part of the comparison contract.
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
    for (_, (old, new)) in panes {
        let card = old.or(new).unwrap().id;
        let previous = epoch(db, card)?;
        if old.is_some() && previous.is_none() {
            return Err("The canonical pane has no placement epoch".into());
        }
        if let (Some(old), Some(new)) = (old, new)
            && before.scope == after.scope
            && old.pending_terminal_start == new.pending_terminal_start
            && old.scope == new.scope
            && same_source(&old.kind, &new.kind)
        {
            continue;
        }
        match previous {
            Some(previous) => {
                let next = previous
                    .checked_add(1)
                    .ok_or("The canonical pane placement epoch is exhausted")?;
                let changed = db
                    .execute(
                        "UPDATE channel_work_placement_epochs SET epoch=?2 WHERE pane=?1 AND epoch=?3",
                        params![card.to_string(), next, previous],
                    )
                    .map_err(|error| error.to_string())?;
                if changed != 1 {
                    return Err("The canonical pane placement epoch changed concurrently".into());
                }
            }
            None if old.is_none() => {
                db.execute(
                    "INSERT INTO channel_work_placement_epochs(pane,epoch) VALUES(?1,1)",
                    [card.to_string()],
                )
                .map_err(|error| error.to_string())?;
            }
            None => return Err("The canonical pane has no placement epoch".into()),
        }
    }
    Ok(())
}

/// An opaque public comparison token. A removed pane's retained token alone
/// grants nothing: resolution must still prove its unique current placement.
pub(in crate::host::model) fn token(db: &Connection, card: CardId) -> Result<u64, String> {
    let value = epoch(db, card)?.ok_or("The canonical pane has no placement epoch")?;
    Ok(TOKEN_TAG | value as u64)
}

#[cfg(test)]
mod tests;
