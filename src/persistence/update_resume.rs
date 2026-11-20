//! Which conversations the windows have taken off an update's offer to
//! resume the work it stopped.
//!
//! Every window reads and writes one store, apart from their own window
//! databases, because one conversation can show in several windows: a
//! conversation resumed in one must not be resumed again from another's
//! offer. Each change is one immediate transaction, so of two windows taking
//! the same conversation only one is told it was first.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::Mutex,
};

use rusqlite::{Connection, OpenFlags, TransactionBehavior, params};

use super::{
    DEFAULT_BUSY_TIMEOUT, Result, StoreError, configure_connection, enforce_private_database,
    enforce_sqlite_sidecars, prepare_private_database,
};
use crate::model::TerminalId;

const SCHEMA: u32 = 1;

/// Older updates whose records are kept, besides the one being written.
const RETAINED_UPDATES: usize = 3;

/// What became of a request to take conversations off an update's offer.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Taken {
    /// Those this request took off, which no window had taken before.
    pub first: Vec<TerminalId>,
    /// Every conversation now off the offer, from any window.
    pub handled: Vec<TerminalId>,
}

/// The shared record, opened on first use and kept open: SQLite's locks on
/// a file are dropped when any descriptor of it closes, so the process keeps
/// a single connection rather than one per request.
pub struct UpdateResumeStore {
    path: PathBuf,
    connection: Mutex<Option<Connection>>,
}

impl UpdateResumeStore {
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            connection: Mutex::new(None),
        }
    }

    /// The conversations no window offers any more for the update `job`.
    pub fn handled(&self, job: &str) -> Result<Vec<TerminalId>> {
        self.with(|connection| handled(connection, job))
    }

    /// Takes `ids` off the offer of the update `job` in every window, and
    /// says which no window had taken before.
    pub fn take(&self, job: &str, ids: &[TerminalId]) -> Result<Taken> {
        self.with(|connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let mut first = Vec::new();
            for id in ids {
                let added = transaction.execute(
                    "INSERT OR IGNORE INTO handled (job, conversation) VALUES (?1, ?2)",
                    params![job, id.to_string()],
                )?;
                if added > 0 && !first.contains(id) {
                    first.push(*id);
                }
            }
            // Only the latest update is offered; a few before it are kept for
            // a window that has yet to read the newer one.
            transaction.execute(
                "DELETE FROM handled WHERE job NOT IN (
                    SELECT job FROM handled GROUP BY job
                    ORDER BY job = ?1 DESC, MAX(rowid) DESC LIMIT ?2
                )",
                params![job, RETAINED_UPDATES + 1],
            )?;
            let handled = handled(&transaction, job)?;
            transaction.commit()?;
            Ok(Taken { first, handled })
        })
    }

    /// Puts `ids` back on the offer of `job`: they were taken to resume, and
    /// could not take the message after all.
    pub fn restore(&self, job: &str, ids: &[TerminalId]) -> Result<()> {
        self.with(|connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            for id in ids {
                transaction.execute(
                    "DELETE FROM handled WHERE job = ?1 AND conversation = ?2",
                    params![job, id.to_string()],
                )?;
            }
            transaction.commit()?;
            Ok(())
        })
    }

    fn with<T>(&self, work: impl FnOnce(&mut Connection) -> Result<T>) -> Result<T> {
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| super::invalid_state!("update resume store lock poisoned"))?;
        if connection.is_none() {
            *connection = Some(open(&self.path)?);
        }
        let result = work(connection.as_mut().expect("opened above"));
        // An error may have come from the connection itself: open afresh
        // next time rather than keep using it.
        if result.is_err() {
            *connection = None;
        }
        result
    }
}

fn open(path: &Path) -> Result<Connection> {
    prepare_private_database(path)?;
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    configure_connection(&connection, DEFAULT_BUSY_TIMEOUT)?;
    enforce_sqlite_sidecars(path)?;
    let found: u32 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if found > SCHEMA {
        return Err(StoreError::UnsupportedSchema {
            found,
            supported: SCHEMA,
        });
    }
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS handled (
            job TEXT NOT NULL,
            conversation TEXT NOT NULL,
            PRIMARY KEY (job, conversation)
        );
        PRAGMA user_version = 1;",
    )?;
    enforce_private_database(path)?;
    enforce_sqlite_sidecars(path)?;
    Ok(connection)
}

fn handled(connection: &Connection, job: &str) -> Result<Vec<TerminalId>> {
    let mut statement =
        connection.prepare("SELECT conversation FROM handled WHERE job = ?1 ORDER BY rowid")?;
    let mut rows = statement.query([job])?;
    let mut ids = Vec::new();
    let mut seen = HashSet::new();
    while let Some(row) = rows.next()? {
        let value: String = row.get(0)?;
        // A row this build cannot read is another build's, not this offer's.
        if let Ok(id) = value.parse::<TerminalId>()
            && seen.insert(id)
        {
            ids.push(id);
        }
    }
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two stores on one file stand for two windows' processes.
    #[test]
    fn of_two_windows_taking_a_conversation_only_one_is_first() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("update-resume.sqlite3");
        let one = UpdateResumeStore::at(&path);
        let two = UpdateResumeStore::at(&path);
        let (a, b) = (TerminalId::new(), TerminalId::new());
        assert_eq!(one.handled("update-1").unwrap(), []);

        let taken = one.take("update-1", &[a]).unwrap();
        assert_eq!(taken.first, [a]);
        assert_eq!(taken.handled, [a]);
        // The other window's offer still lists `a`; taking it again is not
        // first, and its answer says everything already taken.
        let taken = two.take("update-1", &[a, b]).unwrap();
        assert_eq!(taken.first, [b]);
        assert_eq!(taken.handled, [a, b]);
        assert_eq!(one.handled("update-1").unwrap(), [a, b]);
        // Another update's offer is its own.
        assert_eq!(two.handled("update-2").unwrap(), []);

        // One that could not take its message goes back on offer.
        one.restore("update-1", &[b]).unwrap();
        assert_eq!(two.handled("update-1").unwrap(), [a]);
        assert_eq!(two.take("update-1", &[b]).unwrap().first, [b]);
    }

    #[test]
    fn only_the_latest_few_updates_are_kept() {
        let directory = tempfile::tempdir().unwrap();
        let store = UpdateResumeStore::at(directory.path().join("update-resume.sqlite3"));
        let id = TerminalId::new();
        for job in 0..6 {
            store.take(&format!("update-{job}"), &[id]).unwrap();
        }
        // Taking for an older update keeps it, and drops the oldest other.
        store.take("update-1", &[TerminalId::new()]).unwrap();
        let kept: Vec<_> = (0..6)
            .filter(|job| !store.handled(&format!("update-{job}")).unwrap().is_empty())
            .collect();
        assert_eq!(kept, [1, 3, 4, 5]);
    }

    #[test]
    fn a_newer_schema_is_refused_rather_than_rewritten() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("update-resume.sqlite3");
        UpdateResumeStore::at(&path).handled("update-1").unwrap();
        Connection::open(&path)
            .unwrap()
            .pragma_update(None, "user_version", SCHEMA + 1)
            .unwrap();
        assert!(matches!(
            UpdateResumeStore::at(&path).handled("update-1"),
            Err(StoreError::UnsupportedSchema { .. })
        ));
    }
}
