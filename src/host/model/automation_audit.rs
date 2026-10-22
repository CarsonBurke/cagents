//! Lifecycle receipts share the exact state transaction. No inferred history,
//! user-supplied actor, diagnostic, prompt or environment enters this table.
use rusqlite::{Connection, Transaction, params};
use uuid::Uuid;

use super::Registry;
use crate::automations::{
    Automation, LifecycleCause, LifecycleEvent, LifecyclePage, MAX_HISTORY_PAGE,
    MAX_LIFECYCLE_EVENTS,
};

pub(super) fn migrate(tx: &Transaction<'_>) -> Result<(), String> {
    // Deliberately do not manufacture events for existing definitions.
    tx.execute_batch(
        "CREATE TABLE automation_lifecycle (
            sequence INTEGER PRIMARY KEY AUTOINCREMENT,
            automation TEXT NOT NULL REFERENCES automations(id),
            record TEXT NOT NULL
         ) STRICT;
         CREATE INDEX automation_lifecycle_pages ON automation_lifecycle(automation, sequence DESC);",
    )
    .map_err(|error| error.to_string())
}

pub(super) fn record(
    tx: &Transaction<'_>,
    automation: &Automation,
    cause: LifecycleCause,
    firing: Option<Uuid>,
) -> Result<(), String> {
    let event = LifecycleEvent {
        sequence: 0, // The committed row supplies its unique monotonic sequence.
        automation: automation.id,
        revision: automation.revision,
        occurred_ms: automation.updated_ms,
        cause,
        actor: cause.actor(),
        enabled: automation.enabled,
        blocked: automation.blocked.is_some(),
        firing,
    };
    tx.execute(
        "INSERT INTO automation_lifecycle(automation,record) VALUES(?1,?2)",
        params![
            automation.id.to_string(),
            serde_json::to_string(&event).map_err(|e| e.to_string())?
        ],
    )
    .map_err(|error| error.to_string())?;
    tx.execute(
        "DELETE FROM automation_lifecycle WHERE automation=?1 AND sequence NOT IN
         (SELECT sequence FROM automation_lifecycle WHERE automation=?1 ORDER BY sequence DESC LIMIT ?2)",
        params![automation.id.to_string(), MAX_LIFECYCLE_EVENTS],
    )
    .map_err(|error| error.to_string())?;
    Ok(())
}

impl Registry {
    /// Includes tombstones, allowing same-project audit after removal without
    /// returning the removed definition or any launch configuration.
    pub(crate) fn automation_audit_repository(
        &self,
        id: Uuid,
    ) -> Result<std::path::PathBuf, String> {
        self.db
            .query_row(
                "SELECT repository FROM automations WHERE id=?1",
                [id.to_string()],
                |row| row.get::<_, String>(0).map(std::path::PathBuf::from),
            )
            .map_err(|_| "Automation was not found".to_owned())
    }

    pub(crate) fn automation_audit(
        &self,
        id: Uuid,
        before: Option<u64>,
        limit: usize,
    ) -> Result<LifecyclePage, String> {
        // A consistent read transaction keeps retention/creation evidence and
        // this page at one snapshot across independent writer connections.
        let tx = self.db.unchecked_transaction().map_err(|e| e.to_string())?;
        self.automation_audit_repository(id)?;
        let page = read_page(&tx, id, before, limit)?;
        tx.commit().map_err(|e| e.to_string())?;
        Ok(page)
    }
}

fn read_page(
    db: &Connection,
    id: Uuid,
    before: Option<u64>,
    limit: usize,
) -> Result<LifecyclePage, String> {
    let before = before
        .map(i64::try_from)
        .transpose()
        .map_err(|_| "Invalid lifecycle cursor")?;
    let limit = limit.clamp(1, MAX_HISTORY_PAGE);
    let mut query = db
        .prepare(
            "SELECT sequence,record FROM automation_lifecycle WHERE automation=?1
         AND (?2 IS NULL OR sequence < ?2) ORDER BY sequence DESC LIMIT ?3",
        )
        .map_err(|e| e.to_string())?;
    let rows = query
        .query_map(params![id.to_string(), before, limit + 1], |row| {
            Ok((row.get::<_, u64>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|e| e.to_string())?;
    let mut events = rows
        .map(|row| {
            let (sequence, record) = row.map_err(|e| e.to_string())?;
            let mut event: LifecycleEvent =
                serde_json::from_str(&record).map_err(|e| e.to_string())?;
            if event.automation != id || event.actor != event.cause.actor() {
                return Err("Stored lifecycle receipt identity is invalid".into());
            }
            event.sequence = sequence;
            Ok(event)
        })
        .collect::<Result<Vec<_>, String>>()?;
    let more = events.len() > limit;
    events.truncate(limit);
    let next = more.then(|| events.last().unwrap().sequence);
    let creation_recorded: bool = db
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM automation_lifecycle WHERE automation=?1
         AND json_extract(record,'$.cause')='created')",
            [id.to_string()],
            |row| row.get(0),
        )
        .map_err(|e| e.to_string())?;
    Ok(LifecyclePage {
        events,
        next,
        earlier_events_unavailable: !creation_recorded,
    })
}
