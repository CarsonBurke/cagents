//! Per-connection change inventory for execution transactions.
//!
//! Execution writes also occur in transaction helpers shared with retained
//! actors and automations. Temporary triggers observe those writes at their
//! common SQL boundary. The inventory commits and rolls back with the rows it
//! describes, never changes the durable store layout, and reads only touched
//! identities when the execution service publishes an edge.

use super::{Registry, execution};
use crate::{
    model::{AgentId, CardId},
    workspace_model::{
        delta::{EditEffects, ViewRevision},
        views::Scope,
    },
};
use rusqlite::{Connection, OptionalExtension};

pub(super) fn install(db: &Connection) -> Result<(), String> {
    db.execute_batch(
        "CREATE TEMP TABLE execution_view_changes(
            scope_key TEXT PRIMARY KEY,
            channel TEXT, repository TEXT, worktree TEXT,
            before_revision INTEGER, after_revision INTEGER
        ) STRICT;
        CREATE TEMP TABLE execution_closed_changes(pane TEXT PRIMARY KEY) STRICT;
        CREATE TEMP TABLE execution_agent_changes(agent TEXT PRIMARY KEY) STRICT;
        CREATE TEMP TABLE execution_channel_changes(channel TEXT PRIMARY KEY) STRICT;
        CREATE TEMP TRIGGER execution_view_insert AFTER INSERT ON main.view_scopes BEGIN
            INSERT INTO execution_view_changes VALUES(
                json_array(NEW.channel,NEW.repository,NEW.worktree),
                NEW.channel,NEW.repository,NEW.worktree,NULL,NEW.revision)
            ON CONFLICT(scope_key) DO UPDATE SET after_revision=excluded.after_revision;
        END;
        CREATE TEMP TRIGGER execution_view_update AFTER UPDATE OF revision ON main.view_scopes
        WHEN OLD.revision <> NEW.revision BEGIN
            INSERT INTO execution_view_changes VALUES(
                json_array(NEW.channel,NEW.repository,NEW.worktree),
                NEW.channel,NEW.repository,NEW.worktree,OLD.revision,NEW.revision)
            ON CONFLICT(scope_key) DO UPDATE SET after_revision=excluded.after_revision;
        END;
        CREATE TEMP TRIGGER execution_view_delete AFTER DELETE ON main.view_scopes BEGIN
            INSERT INTO execution_view_changes VALUES(
                json_array(OLD.channel,OLD.repository,OLD.worktree),
                OLD.channel,OLD.repository,OLD.worktree,OLD.revision,NULL)
            ON CONFLICT(scope_key) DO UPDATE SET after_revision=NULL;
        END;
        CREATE TEMP TRIGGER execution_pane_delete AFTER DELETE ON main.panes BEGIN
            INSERT OR IGNORE INTO execution_closed_changes VALUES(OLD.id);
        END;
        CREATE TEMP TRIGGER execution_agent_insert AFTER INSERT ON main.agents BEGIN
            INSERT OR IGNORE INTO execution_agent_changes VALUES(NEW.id);
        END;
        CREATE TEMP TRIGGER execution_agent_update AFTER UPDATE ON main.agents
        WHEN OLD.record IS NOT NEW.record OR OLD.closed <> NEW.closed BEGIN
            INSERT OR IGNORE INTO execution_agent_changes VALUES(NEW.id);
        END;
        CREATE TEMP TRIGGER execution_run_insert AFTER INSERT ON main.agent_runs BEGIN
            INSERT OR IGNORE INTO execution_agent_changes VALUES(NEW.agent);
        END;
        CREATE TEMP TRIGGER execution_run_update AFTER UPDATE OF record ON main.agent_runs
        WHEN OLD.record IS NOT NEW.record BEGIN
            INSERT OR IGNORE INTO execution_agent_changes VALUES(NEW.agent);
        END;
        CREATE TEMP TRIGGER execution_agent_delete AFTER DELETE ON main.agents BEGIN
            INSERT OR IGNORE INTO execution_agent_changes VALUES(OLD.id);
        END;
        CREATE TEMP TRIGGER execution_channel_insert AFTER INSERT ON main.channels BEGIN
            INSERT OR IGNORE INTO execution_channel_changes VALUES(NEW.id);
        END;
        CREATE TEMP TRIGGER execution_channel_update AFTER UPDATE ON main.channels
        WHEN OLD.repository IS NOT NEW.repository OR OLD.title IS NOT NEW.title
            OR OLD.ordinal <> NEW.ordinal OR OLD.title_source <> NEW.title_source BEGIN
            INSERT OR IGNORE INTO execution_channel_changes VALUES(NEW.id);
        END;",
    )
    .map_err(|error| error.to_string())
}

const CLEAR: &str = "DELETE FROM execution_view_changes;
    DELETE FROM execution_closed_changes;
    DELETE FROM execution_agent_changes;
    DELETE FROM execution_channel_changes;";

/// Clear the inventory inside a public edit's own transaction, so it commits
/// or rolls back with the edit.
pub(crate) fn discard_execution_delta_in(db: &rusqlite::Connection) -> Result<(), String> {
    db.execute_batch(CLEAR).map_err(|error| error.to_string())
}

impl Registry {
    /// Public model edits publish their own precise effects. Their temporary
    /// inventory must not be repeated by a later execution announcement.
    pub(crate) fn discard_execution_delta(&self) -> Result<(), String> {
        discard_execution_delta_in(&self.db)
    }

    /// Drain all execution changes committed through this connection, pinned
    /// to one snapshot. Keep the registry mutex through publication so a second
    /// announcer cannot publish a later edge ahead of this one.
    pub(crate) fn execution_delta(&mut self) -> Result<(u64, EditEffects), String> {
        let tx = self.db.transaction().map_err(|error| error.to_string())?;
        let sequence: u64 = tx
            .query_row("SELECT sequence FROM metadata WHERE key=0", [], |row| {
                row.get(0)
            })
            .map_err(|error| error.to_string())?;
        let rows = tx
            .prepare(
                "SELECT channel,repository,worktree,before_revision,after_revision
                FROM execution_view_changes ORDER BY scope_key",
            )
            .map_err(|error| error.to_string())?
            .query_map([], |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<u64>>(3)?,
                    row.get::<_, Option<u64>>(4)?,
                ))
            })
            .and_then(Iterator::collect::<rusqlite::Result<Vec<_>>>)
            .map_err(|error| error.to_string())?;
        let mut effects = EditEffects::default();
        for (channel, repository, worktree, before, after) in rows {
            if before == after {
                continue;
            }
            let scope = match (channel, repository, worktree) {
                (Some(channel), None, None) => Scope::Channel(super::parse_channel_id(&channel)?),
                (None, Some(repository), Some(worktree)) => Scope::Worktree {
                    repository: repository.into(),
                    path: worktree.into(),
                },
                _ => return Err("An execution change has an invalid scope".into()),
            };
            effects.views.push(ViewRevision {
                scope,
                before,
                after,
            });
        }
        effects.closed = identities::<CardId>(
            &tx,
            "SELECT pane FROM execution_closed_changes
             WHERE NOT EXISTS(SELECT 1 FROM panes WHERE panes.id=execution_closed_changes.pane)
             ORDER BY pane",
        )?;
        for id in identities::<AgentId>(
            &tx,
            "SELECT agent FROM execution_agent_changes ORDER BY agent",
        )? {
            let record: Option<String> = tx
                .query_row(
                    "SELECT record FROM agents WHERE id=?1 AND closed=0",
                    [id.to_string()],
                    |row| row.get(0),
                )
                .optional()
                .map_err(|error| error.to_string())?;
            match record {
                Some(record) => effects.agents.push(execution::decode(&record)?),
                None => effects.removed_agents.push(id),
            }
        }
        for id in identities(
            &tx,
            "SELECT channel FROM execution_channel_changes ORDER BY channel",
        )? {
            if let Some(channel) = super::channel_row(&tx, id)? {
                effects.channels.push(channel);
            }
        }
        tx.execute_batch(CLEAR).map_err(|error| error.to_string())?;
        tx.commit().map_err(|error| error.to_string())?;
        Ok((sequence, effects))
    }
}

fn identities<T: std::str::FromStr>(db: &Connection, query: &str) -> Result<Vec<T>, String>
where
    T::Err: std::fmt::Display,
{
    db.prepare(query)
        .map_err(|error| error.to_string())?
        .query_map([], |row| row.get::<_, String>(0))
        .and_then(Iterator::collect::<rusqlite::Result<Vec<_>>>)
        .map_err(|error| error.to_string())?
        .into_iter()
        .map(|id| {
            id.parse()
                .map_err(|error| format!("Invalid execution identity: {error}"))
        })
        .collect()
}
