//! INACTIVE history-format integration: no allocated published schema yet.
//! Do not execute or ship this proposal until its opener/migration is joined.
//!
//! A future published migration must import every old tombstone before enabling
//! bounded recent JSON. These indexed guards are permanent for a scope's life;
//! they are not an authority to retire a process or change pane placement.
//!
//! Successful operation identities remain permanent for their scope's life:
//! today's client may rebase the same operation after ViewConflict. Recent
//! receipts may be compacted only after durable indexed identity transfer.

use super::{Scope, View, key};
use crate::model::CardId;
use rusqlite::{Connection, Transaction, params};

mod operations;

pub(super) fn operation_seen(db: &Connection, operation: uuid::Uuid) -> Result<bool, String> {
    operations::seen(db, operation)
}

pub(super) fn compact_operations(tx: &Transaction<'_>) -> Result<(), String> {
    operations::compact(tx)
}

use crate::workspace_model::views::RECENT_RETIRED_LIMIT;

/// Run only inside the opener's already admitted write transaction, after all
/// preceding published migrations. The caller owns rollback, schema admission
/// and final header publication. No production caller exists yet.
pub(super) fn migrate(tx: &Transaction<'_>) -> Result<(), String> {
    let views = super::all(tx)?;
    // Validate before rewriting a history payload. A guard for an active pane
    // would contradict the canonical placement we must preserve.
    for view in &views {
        if view
            .panes
            .iter()
            .any(|pane| view.retired.contains(&pane.id))
        {
            return Err("An active pane also appears in its retired history".into());
        }
    }
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS retired_view_panes(
            scope TEXT NOT NULL REFERENCES shared_view_scopes(scope) ON DELETE CASCADE,
            id TEXT NOT NULL, PRIMARY KEY(scope, id)
        ) STRICT;
        INSERT OR IGNORE INTO retired_view_panes(scope,id)
            SELECT shared_views.scope, retired.value
            FROM shared_views, json_each(shared_views.retired) AS retired;",
    )
    .map_err(|error| error.to_string())?;
    let mut trimmed = false;
    for mut view in views {
        if trim_recent(&mut view) {
            let revision = view
                .revision
                .checked_add(1)
                .ok_or("view revision exhausted")?;
            // History-only migration must not call general view publication:
            // leave panes, title inference, CoreHome and placement epochs alone.
            tx.execute(
                "UPDATE shared_views SET revision=?2, retired=?3 WHERE scope=?1",
                params![
                    key(&view.scope)?,
                    revision,
                    serde_json::to_string(&view.retired).map_err(|error| error.to_string())?
                ],
            )
            .map_err(|error| error.to_string())?;
            trimmed = true;
        }
    }
    if trimmed {
        tx.execute("UPDATE metadata SET sequence=sequence+1 WHERE key=0", [])
            .map_err(|error| error.to_string())?;
    }
    operations::migrate(tx)
}

pub(super) fn contains(db: &Connection, scope: &Scope, id: CardId) -> Result<bool, String> {
    db.query_row(
        "SELECT EXISTS(SELECT 1 FROM retired_view_panes WHERE scope=?1 AND id=?2)",
        params![key(scope)?, id.to_string()],
        |row| row.get(0),
    )
    .map_err(|error| error.to_string())
}

pub(super) fn retire(db: &Connection, scope: &Scope, id: CardId) -> Result<(), String> {
    require_transaction(db)?;
    db.execute(
        "INSERT OR IGNORE INTO retired_view_panes(scope,id) VALUES(?1,?2)",
        params![key(scope)?, id.to_string()],
    )
    .map(drop)
    .map_err(|error| error.to_string())
}

/// Only an explicitly authorized atomic transfer may clear a destination guard.
/// The source retirement, agent placement, provenance and this deletion must
/// share the existing canonical view transaction; failures roll back together.
pub(super) fn allow_transfer_back(
    db: &Connection,
    scope: &Scope,
    id: CardId,
) -> Result<(), String> {
    require_transaction(db)?;
    db.execute(
        "DELETE FROM retired_view_panes WHERE scope=?1 AND id=?2",
        params![key(scope)?, id.to_string()],
    )
    .map(drop)
    .map_err(|error| error.to_string())
}

fn require_transaction(db: &Connection) -> Result<(), String> {
    if db.is_autocommit() {
        return Err("Retirement guards require the canonical view transaction".into());
    }
    Ok(())
}

pub(super) fn trim_recent(view: &mut View) -> bool {
    if view.retired.len() <= RECENT_RETIRED_LIMIT {
        return false;
    }
    view.retired
        .drain(..view.retired.len() - RECENT_RETIRED_LIMIT);
    true
}

// The four current19 helper-only oracles remain frozen in the separate36d
// source layer. Allocated-opener migration tests replace those legacy fixtures.
#[cfg(test)]
mod fixtures;

// Allocation-dependent assembled view/retirement tests. Do not execute this
// proposal as schema19: the production opener has not created its guards.
#[cfg(test)]
mod integration_tests;
