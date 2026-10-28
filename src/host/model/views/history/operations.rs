//! Compact recent receipt rows without forgetting committed operation identity.
//!
//! The old receipt was already only UUID text plus scope text, not an edit
//! payload. The archive stores a 16-byte UUID and one interned integer scope ID
//! instead of repeating both strings. Identity count still grows until scope
//! deletion; this is not a bound on total database bytes or operation count.

use super::require_transaction;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use uuid::Uuid;

pub(super) const RECENT_OPERATIONS: usize = 2048;
const MIGRATION_BATCH: usize = 256;

pub(super) fn migrate(tx: &Transaction<'_>) -> Result<(), String> {
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS compacted_view_operation_scopes(
            id INTEGER PRIMARY KEY,
            scope TEXT NOT NULL UNIQUE REFERENCES shared_view_scopes(scope) ON DELETE CASCADE
        ) STRICT;
        CREATE TABLE IF NOT EXISTS compacted_view_operations(
            operation BLOB PRIMARY KEY CHECK(length(operation)=16),
            scope INTEGER NOT NULL REFERENCES compacted_view_operation_scopes(id) ON DELETE CASCADE
        ) STRICT, WITHOUT ROWID;
        CREATE INDEX IF NOT EXISTS compacted_view_operations_by_scope
            ON compacted_view_operations(scope);",
    )
    .map_err(|error| error.to_string())?;
    compact(tx)
}

/// Used by BOTH Host preflight and the transactional view writer. Archive
/// identity has the same global-UUID no-op semantics as a recent receipt,
/// including UUID reuse with another scope/change. It grants no new mutation.
pub(super) fn seen(db: &Connection, operation: Uuid) -> Result<bool, String> {
    db.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM shared_view_edits WHERE operation=?1
            UNION ALL
            SELECT 1 FROM compacted_view_operations WHERE operation=?2
        )",
        params![operation.to_string(), operation.as_bytes().as_slice()],
        |row| row.get(0),
    )
    .map_err(|error| error.to_string())
}

/// The receipt insert, archive insertion and recent-row deletion share the
/// caller's admitted write transaction. A failure rolls everything back.
pub(super) fn compact(tx: &Transaction<'_>) -> Result<(), String> {
    require_transaction(tx)?;
    let cutoff: Option<i64> = tx
        .query_row(
            "SELECT rowid FROM shared_view_edits ORDER BY rowid DESC LIMIT 1 OFFSET ?1",
            [RECENT_OPERATIONS],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| error.to_string())?;
    let Some(cutoff) = cutoff else {
        return Ok(());
    };
    loop {
        // Bound migration staging by row count. Scope-string bytes remain
        // proportional to admitted existing data; no absolute byte cap claimed.
        let batch = tx
            .prepare(
                "SELECT rowid,operation,scope FROM shared_view_edits
             WHERE rowid<=?1 ORDER BY rowid LIMIT ?2",
            )
            .map_err(|error| error.to_string())?
            .query_map(params![cutoff, MIGRATION_BATCH], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .map_err(|error| error.to_string())?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|error| error.to_string())?;
        let Some((last, _, _)) = batch.last() else {
            return Ok(());
        };
        for (_, encoded, scope) in &batch {
            let operation = Uuid::parse_str(encoded)
                .map_err(|_| "Cannot compact an invalid workspace operation UUID")?;
            if operation.to_string() != *encoded {
                return Err("Cannot compact a noncanonical workspace operation UUID".into());
            }
            tx.execute(
                "INSERT INTO compacted_view_operation_scopes(scope) VALUES(?1) ON CONFLICT(scope) DO NOTHING",
                [scope],
            ).map_err(|error| error.to_string())?;
            tx.execute(
                "INSERT INTO compacted_view_operations(operation,scope)
                 SELECT ?1,id FROM compacted_view_operation_scopes WHERE scope=?2
                 ON CONFLICT(operation) DO NOTHING",
                params![operation.as_bytes().as_slice(), scope],
            )
            .map_err(|error| error.to_string())?;
            let archived: String = tx
                .query_row(
                    "SELECT s.scope FROM compacted_view_operations o
                 JOIN compacted_view_operation_scopes s ON s.id=o.scope WHERE o.operation=?1",
                    [operation.as_bytes().as_slice()],
                    |row| row.get(0),
                )
                .map_err(|error| error.to_string())?;
            if &archived != scope {
                return Err("The compacted workspace operation belongs to another scope".into());
            }
        }
        // Every selected identity is now durably staged in this same TX.
        tx.execute("DELETE FROM shared_view_edits WHERE rowid<=?1", [last])
            .map_err(|error| error.to_string())?;
    }
}

#[cfg(test)]
mod tests;
