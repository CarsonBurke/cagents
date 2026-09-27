//! Single-writer durable attention, independent of workspace/card persistence.
use std::path::Path;

use rusqlite::{Connection, OptionalExtension, params};
use uuid::Uuid;

use crate::notifications::{Event, Notice, Target};

const MAX_SESSIONS: usize = 2048;
const KEYS_PER_SESSION: usize = 32;

pub(super) struct Journal(Connection);

impl Journal {
    pub fn open(path: &Path) -> Result<Self, String> {
        drop(
            crate::secure_file::open_private_append_file(path)
                .map_err(|error| error.to_string())?,
        );
        let connection = Connection::open(path).map_err(|error| error.to_string())?;
        let version: u32 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(|error| error.to_string())?;
        if version > 2 {
            return Err("Notification journal was created by a newer cagents version".into());
        }
        connection
            .busy_timeout(std::time::Duration::from_secs(1))
            .map_err(|error| error.to_string())?;
        connection.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
            CREATE TABLE IF NOT EXISTS notices (
                session TEXT PRIMARY KEY, id TEXT NOT NULL, event TEXT NOT NULL,
                delivered INTEGER NOT NULL, acknowledged INTEGER NOT NULL,
                ordinal INTEGER NOT NULL);
            CREATE TABLE IF NOT EXISTS seen (
                session TEXT NOT NULL, evidence TEXT NOT NULL, ordinal INTEGER PRIMARY KEY AUTOINCREMENT,
                UNIQUE(session, evidence));
            CREATE TABLE IF NOT EXISTS owners (session TEXT PRIMARY KEY, window TEXT NOT NULL, ordinal INTEGER NOT NULL);
            PRAGMA user_version=2;").map_err(|error| error.to_string())?;
        Ok(Self(connection))
    }

    /// Coalesce a session's latest attention while retaining a bounded replay
    /// ledger. A duplicate must not resurrect a dismissed notification.
    pub fn record(&mut self, event: Event, suppressed: bool) -> Result<Option<Notice>, String> {
        if event.evidence.key.len() > 1024 || event.directory.as_os_str().len() > 4096 {
            return Err("Agent attention metadata exceeds its size limit".into());
        }
        let session = event.target().storage_key();
        let evidence = serde_json::to_string(&event.evidence).map_err(|error| error.to_string())?;
        let serialized = serde_json::to_string(&event).map_err(|error| error.to_string())?;
        let transaction = self.0.transaction().map_err(|error| error.to_string())?;
        let inserted = transaction
            .execute(
                "INSERT OR IGNORE INTO seen(session,evidence) VALUES (?1,?2)",
                params![session, evidence],
            )
            .map_err(|error| error.to_string())?;
        if inserted == 0 {
            return Ok(None);
        }
        let ordinal = transaction.last_insert_rowid();
        let owner = event
            .source_session()
            .map(|id| id.to_string())
            .unwrap_or_else(|| session.clone());
        let window = transaction
            .query_row(
                "SELECT window FROM owners WHERE session=?1",
                [&owner],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| error.to_string())?;
        if let Some(window) = &window {
            transaction.execute("INSERT INTO owners(session,window,ordinal) VALUES (?1,?2,(SELECT coalesce(max(ordinal),0)+1 FROM owners)) ON CONFLICT(session) DO UPDATE SET window=excluded.window,ordinal=excluded.ordinal", params![session, window]).map_err(|error| error.to_string())?;
        }
        let notice = Notice {
            id: Uuid::new_v4(),
            event,
            delivered: suppressed,
            acknowledged: false,
            window,
        };
        transaction.execute("INSERT INTO notices(session,id,event,delivered,acknowledged,ordinal) VALUES (?1,?2,?3,?4,0,?5)
            ON CONFLICT(session) DO UPDATE SET id=excluded.id,event=excluded.event,delivered=excluded.delivered,acknowledged=0,ordinal=excluded.ordinal",
            params![session, notice.id.to_string(), serialized, suppressed, ordinal]).map_err(|error| error.to_string())?;
        transaction.execute("DELETE FROM seen WHERE session=?1 AND ordinal NOT IN (SELECT ordinal FROM seen WHERE session=?1 ORDER BY ordinal DESC LIMIT ?2)", params![session, KEYS_PER_SESSION]).map_err(|error| error.to_string())?;
        transaction.execute("DELETE FROM notices WHERE session NOT IN (SELECT session FROM notices ORDER BY ordinal DESC LIMIT ?1)", [MAX_SESSIONS]).map_err(|error| error.to_string())?;
        transaction
            .execute(
                "DELETE FROM seen WHERE session NOT IN (SELECT session FROM notices)",
                [],
            )
            .map_err(|error| error.to_string())?;
        transaction.execute("DELETE FROM owners WHERE session NOT IN (SELECT session FROM owners ORDER BY ordinal DESC LIMIT ?1)", [MAX_SESSIONS]).map_err(|error| error.to_string())?;
        transaction.commit().map_err(|error| error.to_string())?;
        Ok(Some(notice))
    }

    pub fn list(&self) -> Result<Vec<Notice>, String> {
        let mut statement = self.0.prepare("SELECT id,event,delivered,acknowledged,(SELECT window FROM owners WHERE owners.session=notices.session) FROM notices ORDER BY ordinal DESC").map_err(|error| error.to_string())?;
        statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, bool>(2)?,
                    row.get::<_, bool>(3)?,
                    row.get::<_, Option<String>>(4)?,
                ))
            })
            .map_err(|error| error.to_string())?
            .map(|row| {
                let (id, event, delivered, acknowledged, window) =
                    row.map_err(|error| error.to_string())?;
                Ok(Notice {
                    id: id.parse().map_err(|error: uuid::Error| error.to_string())?,
                    event: Event::from_journal(&event).map_err(|error| error.to_string())?,
                    delivered,
                    acknowledged,
                    window,
                })
            })
            .collect()
    }

    pub fn delivered(&self, id: Uuid) -> Result<(), String> {
        self.0
            .execute(
                "UPDATE notices SET delivered=1 WHERE id=?1",
                [id.to_string()],
            )
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    pub fn acknowledge(&self, id: Uuid) -> Result<Option<String>, String> {
        let session = self
            .0
            .query_row(
                "SELECT session FROM notices WHERE id=?1",
                [id.to_string()],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| error.to_string())?;
        self.0
            .execute(
                "UPDATE notices SET acknowledged=1,delivered=1 WHERE id=?1",
                [id.to_string()],
            )
            .map_err(|error| error.to_string())?;
        Ok(session)
    }

    pub fn retire(&mut self, target: Target) -> Result<(), String> {
        let transaction = self.0.transaction().map_err(|error| error.to_string())?;
        transaction
            .execute(
                "DELETE FROM notices WHERE session=?1",
                [target.storage_key()],
            )
            .map_err(|error| error.to_string())?;
        transaction
            .execute("DELETE FROM seen WHERE session=?1", [target.storage_key()])
            .map_err(|error| error.to_string())?;
        transaction
            .execute(
                "DELETE FROM owners WHERE session=?1",
                [target.storage_key()],
            )
            .map_err(|error| error.to_string())?;
        transaction.commit().map_err(|error| error.to_string())
    }

    pub fn remember_window(
        &mut self,
        sessions: impl IntoIterator<Item = crate::model::TerminalId>,
        window: &str,
    ) -> Result<(), String> {
        let transaction = self.0.transaction().map_err(|error| error.to_string())?;
        for session in sessions {
            transaction.execute("INSERT INTO owners(session,window,ordinal) VALUES (?1,?2,(SELECT coalesce(max(ordinal),0)+1 FROM owners)) ON CONFLICT(session) DO UPDATE SET window=excluded.window,ordinal=excluded.ordinal", params![session.to_string(), window]).map_err(|error| error.to_string())?;
        }
        transaction.execute("DELETE FROM owners WHERE session NOT IN (SELECT session FROM owners ORDER BY ordinal DESC LIMIT ?1)", [MAX_SESSIONS]).map_err(|error| error.to_string())?;
        transaction.commit().map_err(|error| error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        agent::Provider,
        model::TerminalId,
        notifications::{Evidence, Kind},
    };

    fn event(session: TerminalId, key: &str) -> Event {
        Event {
            target: crate::notifications::Target::Session(session),
            source_session: Some(session),
            provider: Some(Provider::Codex),
            directory: "/project".into(),
            evidence: Evidence {
                key: key.into(),
                kind: Kind::Succeeded,
            },
        }
    }

    #[test]
    fn run_attention_survives_source_retirement_and_replay_keeps_dismissal() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("attention.sqlite3");
        let session = TerminalId::new();
        let mut journal = Journal::open(&path).unwrap();
        journal.remember_window([session], "owned").unwrap();
        let mut run = event(session, "accepted-run");
        run.target = Target::AgentRun(crate::model::RunId::from_uuid(session.as_uuid()));
        let notice = journal.record(run.clone(), false).unwrap().unwrap();
        journal
            .record(event(session, "provider-turn"), false)
            .unwrap();
        journal.retire(Target::Session(session)).unwrap();
        assert_eq!(journal.list().unwrap(), vec![notice.clone()]);
        assert_eq!(notice.window.as_deref(), Some("owned"));
        journal.acknowledge(notice.id).unwrap();
        drop(journal);
        let mut journal = Journal::open(&path).unwrap();
        assert!(journal.record(run, false).unwrap().is_none());
        let stored = journal.list().unwrap();
        assert_eq!(stored.len(), 1);
        assert!(stored[0].acknowledged);
        assert_eq!(stored[0].event.target(), notice.event.target());
    }

    #[test]
    fn version_one_journal_keeps_legacy_attention_and_owner() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("attention.sqlite3");
        let session = TerminalId::new();
        let mut journal = Journal::open(&path).unwrap();
        journal.remember_window([session], "saved").unwrap();
        let notice = journal
            .record(event(session, "old"), true)
            .unwrap()
            .unwrap();
        let legacy = serde_json::json!({"session":session,"provider":Provider::Codex,"directory":"/project","evidence":{"key":"old","kind":"Succeeded"}});
        journal
            .0
            .execute("UPDATE notices SET event=?1", [legacy.to_string()])
            .unwrap();
        journal.0.execute_batch("PRAGMA user_version=1").unwrap();
        drop(journal);
        let journal = Journal::open(&path).unwrap();
        assert_eq!(journal.list().unwrap(), vec![notice]);
    }

    #[test]
    fn new_run_owner_uses_owner_recency_even_when_attention_is_infrequent() {
        let directory = tempfile::tempdir().unwrap();
        let mut journal = Journal::open(&directory.path().join("attention.sqlite3")).unwrap();
        let session = TerminalId::new();
        journal.0.execute("WITH RECURSIVE n(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM n WHERE i<?1) INSERT INTO owners(session,window,ordinal) SELECT 'old-'||i,'other',10000+i FROM n", [MAX_SESSIONS - 1]).unwrap();
        journal.remember_window([session], "chosen").unwrap();
        let mut run = event(session, "first-attention");
        run.target = Target::AgentRun(crate::model::RunId::new());
        journal.record(run, false).unwrap();
        assert_eq!(journal.list().unwrap()[0].window.as_deref(), Some("chosen"));
    }

    #[test]
    fn newer_journal_schema_is_rejected_without_schema_changes() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("attention.sqlite3");
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch("PRAGMA user_version=3; CREATE TABLE future(value TEXT);")
            .unwrap();
        drop(connection);
        assert!(Journal::open(&path).err().unwrap().contains("newer"));
        let connection = Connection::open(&path).unwrap();
        let version: u32 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 3);
        let notices: i32 = connection
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name='notices'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(notices, 0);
    }

    #[test]
    fn focus_suppression_and_owner_identity_survive_reconnect_without_resurrection() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("attention.sqlite3");
        let session = TerminalId::new();
        let mut journal = Journal::open(&path).unwrap();
        journal.remember_window([session], "named").unwrap();
        let notice = journal
            .record(event(session, "turn"), true)
            .unwrap()
            .unwrap();
        assert!(notice.delivered);
        assert_eq!(notice.window.as_deref(), Some("named"));
        drop(journal);
        let mut journal = Journal::open(&path).unwrap();
        assert!(
            journal
                .record(event(session, "turn"), false)
                .unwrap()
                .is_none()
        );
        assert_eq!(journal.list().unwrap(), vec![notice]);
        journal.retire(Target::Session(session)).unwrap();
        assert!(journal.list().unwrap().is_empty());
    }

    #[test]
    fn replay_after_restart_never_resurrects_delivered_or_acknowledged_attention() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("attention.sqlite3");
        let session = TerminalId::new();
        let mut journal = Journal::open(&path).unwrap();
        let notice = journal
            .record(event(session, "first"), false)
            .unwrap()
            .unwrap();
        journal.delivered(notice.id).unwrap();
        journal.acknowledge(notice.id).unwrap();
        drop(journal);
        let mut journal = Journal::open(&path).unwrap();
        assert!(
            journal
                .record(event(session, "first"), false)
                .unwrap()
                .is_none()
        );
        assert!(journal.list().unwrap()[0].acknowledged);
        let next = journal
            .record(event(session, "second"), false)
            .unwrap()
            .unwrap();
        assert_ne!(next.id, notice.id);
        assert!(
            journal
                .record(event(session, "first"), false)
                .unwrap()
                .is_none()
        );
        assert_eq!(journal.list().unwrap(), vec![next]);
        journal.retire(Target::Session(session)).unwrap();
        assert!(journal.list().unwrap().is_empty());
    }

    #[test]
    fn ledger_and_session_storage_are_bounded() {
        let directory = tempfile::tempdir().unwrap();
        let mut journal = Journal::open(&directory.path().join("attention.sqlite3")).unwrap();
        let session = TerminalId::new();
        for number in 0..KEYS_PER_SESSION + 5 {
            journal
                .record(event(session, &number.to_string()), true)
                .unwrap();
        }
        let count: usize = journal
            .0
            .query_row("SELECT count(*) FROM seen", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, KEYS_PER_SESSION);
        assert_eq!(journal.list().unwrap().len(), 1);
        assert!(journal.list().unwrap()[0].delivered);
    }
}
