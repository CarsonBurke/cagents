//! A window's durable terminal cards include local-only panes and directories
//! outside shared projects. Notification presence publishes those IDs already;
//! retain their completion pin after disconnect until an explicit close.
use super::Registry;
use crate::model::TerminalId;
use rusqlite::{Connection, TransactionBehavior};

pub(super) fn migrate(db: &Connection) -> Result<(), String> {
    let transaction = rusqlite::Transaction::new_unchecked(db, TransactionBehavior::Immediate)
        .map_err(|error| error.to_string())?;
    transaction.execute_batch("CREATE TABLE IF NOT EXISTS terminal_retention_pins(
        terminal TEXT PRIMARY KEY
    ) STRICT;").map_err(|error| error.to_string())?;
    transaction.pragma_update(None, "user_version", 13).map_err(|error| error.to_string())?;
    transaction.commit().map_err(|error| error.to_string())
}

impl Registry {
    pub(crate) fn pin_window_terminals(&mut self, terminals: &[TerminalId]) -> Result<(), String> {
        let transaction = self.db.transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        for terminal in terminals {
            transaction.execute("INSERT INTO terminal_retention_pins(terminal) VALUES (?1) ON CONFLICT DO NOTHING",
                [terminal.to_string()]).map_err(|error| error.to_string())?;
        }
        transaction.commit().map_err(|error| error.to_string())
    }
    pub(crate) fn retention_pins(&self, candidates: &std::collections::BTreeSet<TerminalId>) -> Result<std::collections::BTreeSet<TerminalId>, String> {
        // Stale closed-window IDs never make the census scale with history:
        // only currently hosted candidates perform indexed point lookups.
        let mut statement = self.db.prepare("SELECT EXISTS(SELECT 1 FROM terminal_retention_pins WHERE terminal=?1)")
            .map_err(|error| error.to_string())?;
        let mut pinned = std::collections::BTreeSet::new();
        for candidate in candidates {
            let exists: bool = statement.query_row([candidate.to_string()], |row| row.get(0))
                .map_err(|error| error.to_string())?;
            if exists { pinned.insert(*candidate); }
        }
        Ok(pinned)
    }
    pub(crate) fn forget_retention_pin(&mut self, terminal: TerminalId) -> Result<(), String> {
        self.db.execute("DELETE FROM terminal_retention_pins WHERE terminal=?1", [terminal.to_string()])
            .map(drop).map_err(|error| error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn local_window_terminal_pins_survive_disconnect_and_registry_restart() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("registry.sqlite3");
        let terminal = TerminalId::new();
        let mut registry = Registry::at(&path).unwrap();
        registry.pin_window_terminals(&[terminal]).unwrap();
        // An early empty presence on reconnect cannot erase saved ownership.
        registry.pin_window_terminals(&[]).unwrap();
        assert!(registry.pinned_terminals(&std::collections::BTreeSet::from([terminal])).unwrap().contains(&terminal));
        drop(registry);
        let mut reopened = Registry::at(&path).unwrap();
        assert!(reopened.pinned_terminals(&std::collections::BTreeSet::from([terminal])).unwrap().contains(&terminal));
        reopened.forget_retention_pin(terminal).unwrap();
        assert!(!reopened.pinned_terminals(&std::collections::BTreeSet::from([terminal])).unwrap().contains(&terminal));
    }
}
