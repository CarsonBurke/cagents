//! Which session started each terminal an agent spawned, so the agent may go
//! on changing it after a reconnect, a relaunch or a daemon restart. Like
//! session lineages, rows are kept: a closed card can still be relaunched.

use super::Registry;
use crate::agent_execution::Lineage;
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

pub(super) fn migrate(db: &Connection) -> Result<(), String> {
    let tx = Transaction::new_unchecked(db, TransactionBehavior::Immediate)
        .map_err(|error| error.to_string())?;
    let current: u32 = tx
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(|error| error.to_string())?;
    if current < 6 {
        tx.execute_batch(
            "CREATE TABLE IF NOT EXISTS terminal_owners(
                 lineage TEXT PRIMARY KEY,
                 owner TEXT NOT NULL
             ) STRICT;
             PRAGMA user_version = 6;",
        )
        .map_err(|error| error.to_string())?;
    }
    tx.commit().map_err(|error| error.to_string())
}

impl Registry {
    /// Records that `owner` started the terminal of `lineage`.
    pub(crate) fn own_terminal(&mut self, lineage: Lineage, owner: Lineage) -> Result<(), String> {
        self.db
            .execute(
                "INSERT INTO terminal_owners(lineage, owner) VALUES (?1, ?2)
                 ON CONFLICT(lineage) DO UPDATE SET owner = excluded.owner",
                params![lineage.to_string(), owner.to_string()],
            )
            .map(drop)
            .map_err(|error| error.to_string())
    }

    /// The session lineage that started the terminal of `lineage`, if an
    /// agent did.
    pub(crate) fn terminal_owner(&self, lineage: Lineage) -> Result<Option<Lineage>, String> {
        self.db
            .query_row(
                "SELECT owner FROM terminal_owners WHERE lineage = ?1",
                [lineage.to_string()],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|error| error.to_string())?
            .map(|owner| {
                owner
                    .parse()
                    .map(Lineage)
                    .map_err(|_| format!("stored terminal owner `{owner}` is invalid"))
            })
            .transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::TerminalId;

    #[test]
    fn an_owner_survives_reopening_the_registry() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("model.sqlite3");
        let mut registry = Registry::at(&path).unwrap();
        let (terminal, owner, later) = (
            Lineage(TerminalId::new()),
            Lineage(TerminalId::new()),
            Lineage(TerminalId::new()),
        );
        assert_eq!(registry.terminal_owner(terminal).unwrap(), None);
        registry.own_terminal(terminal, owner).unwrap();
        drop(registry);
        let mut registry = Registry::at(&path).unwrap();
        assert_eq!(registry.terminal_owner(terminal).unwrap(), Some(owner));
        // A spawn that failed leaves its ID free for whoever starts it next.
        registry.own_terminal(terminal, later).unwrap();
        assert_eq!(registry.terminal_owner(terminal).unwrap(), Some(later));
    }
}
