//! Single-writer durable attention, independent of workspace/card persistence.
use std::path::Path;

use diesel::{connection::SimpleConnection, prelude::*, sqlite::SqliteConnection};
use uuid::Uuid;

use crate::notifications::{Event, Notice, Target};

const MAX_SESSIONS: i64 = 2048;
const KEYS_PER_SESSION: i64 = 32;

mod schema {
    diesel::table! {
        notices (session) {
            session -> Text,
            id -> Text,
            event -> Text,
            delivered -> Bool,
            acknowledged -> Bool,
            ordinal -> BigInt,
        }
    }
    diesel::table! {
        seen (ordinal) {
            session -> Text,
            evidence -> Text,
            ordinal -> BigInt,
        }
    }
    diesel::table! {
        owners (session) {
            session -> Text,
            window -> Text,
            ordinal -> BigInt,
        }
    }
    diesel::table! {
        review_requests_seen (identity) {
            identity -> Text,
        }
    }
    diesel::allow_tables_to_appear_in_same_query!(notices, seen, owners, review_requests_seen);
}
use schema::{notices, owners, review_requests_seen, seen};

// PRAGMAs describe the SQLite connection rather than application rows.
#[derive(diesel::QueryableByName)]
struct SchemaVersion {
    #[diesel(sql_type = diesel::sql_types::Integer)]
    user_version: i32,
}

pub(super) struct Journal(SqliteConnection);

impl Journal {
    pub fn open(path: &Path) -> Result<Self, String> {
        drop(
            crate::secure_file::open_private_append_file(path)
                .map_err(|error| error.to_string())?,
        );
        // A URI preserves non-UTF8 filesystem names and quotes SQLite's URI
        // metacharacters. A lossy display path could open a different database.
        let absolute = std::path::absolute(path).map_err(|error| error.to_string())?;
        let uri = url::Url::from_file_path(absolute)
            .map_err(|()| "Notification journal path cannot be represented as a file URI")?;
        let mut connection =
            SqliteConnection::establish(uri.as_str()).map_err(|error| error.to_string())?;
        let version: SchemaVersion = diesel::sql_query("PRAGMA user_version")
            .get_result(&mut connection)
            .map_err(|error| error.to_string())?;
        if version.user_version > 3 {
            return Err("Notification journal was created by a newer cagents version".into());
        }
        connection
            .batch_execute("PRAGMA busy_timeout=1000;")
            .map_err(|error| error.to_string())?;
        connection
            .batch_execute("PRAGMA journal_mode=WAL;")
            .map_err(|error| error.to_string())?;
        connection
            .batch_execute("PRAGMA synchronous=FULL;")
            .map_err(|error| error.to_string())?;
        connection.batch_execute("CREATE TABLE IF NOT EXISTS notices (
                session TEXT PRIMARY KEY, id TEXT NOT NULL, event TEXT NOT NULL,
                delivered INTEGER NOT NULL, acknowledged INTEGER NOT NULL,
                ordinal INTEGER NOT NULL);
            CREATE TABLE IF NOT EXISTS seen (
                session TEXT NOT NULL, evidence TEXT NOT NULL, ordinal INTEGER PRIMARY KEY AUTOINCREMENT,
                UNIQUE(session, evidence));
            CREATE TABLE IF NOT EXISTS owners (session TEXT PRIMARY KEY, window TEXT NOT NULL, ordinal INTEGER NOT NULL);
            CREATE TABLE IF NOT EXISTS review_requests_seen (identity TEXT PRIMARY KEY);
            PRAGMA user_version=3;").map_err(|error| error.to_string())?;
        Ok(Self(connection))
    }

    /// Coalesce latest attention without resurrecting dismissed duplicates.
    pub fn record(&mut self, event: Event, suppressed: bool) -> Result<Option<Notice>, String> {
        event.validate()?;
        let session = event.target().storage_key();
        let evidence = serde_json::to_string(&event.evidence).map_err(|error| error.to_string())?;
        let serialized = serde_json::to_string(&event).map_err(|error| error.to_string())?;
        let result = self
            .0
            .transaction::<_, diesel::result::Error, _>(|connection| {
                if let Target::PullRequest(request) = &event.target {
                    // Immutable review identity outlives bounded payloads and
                    // dismissal; every new request node receives its own alert.
                    debug_assert!(request.validate().is_ok());
                    let inserted = diesel::insert_into(review_requests_seen::table)
                        .values(review_requests_seen::identity.eq(&session))
                        .on_conflict_do_nothing()
                        .execute(connection)?;
                    if inserted == 0 {
                        return Err(diesel::result::Error::RollbackTransaction);
                    }
                }
                let ordinal = diesel::insert_into(seen::table)
                    .values((seen::session.eq(&session), seen::evidence.eq(&evidence)))
                    .on_conflict_do_nothing()
                    .returning(seen::ordinal)
                    .get_result::<i64>(connection)
                    .optional()?;
                let Some(ordinal) = ordinal else {
                    return Err(diesel::result::Error::RollbackTransaction);
                };
                let owner = event
                    .source_session()
                    .map(|id| id.to_string())
                    .unwrap_or_else(|| session.clone());
                let window = owners::table
                    .find(&owner)
                    .select(owners::window)
                    .first::<String>(connection)
                    .optional()?;
                if let Some(window) = &window {
                    remember_owner(connection, &session, window)?;
                }
                let notice = Notice {
                    id: Uuid::new_v4(),
                    event,
                    delivered: suppressed,
                    acknowledged: false,
                    window,
                };
                let values = (
                    notices::id.eq(notice.id.to_string()),
                    notices::event.eq(&serialized),
                    notices::delivered.eq(suppressed),
                    notices::acknowledged.eq(false),
                    notices::ordinal.eq(ordinal),
                );
                diesel::insert_into(notices::table)
                    .values((notices::session.eq(&session), values.clone()))
                    .on_conflict(notices::session)
                    .do_update()
                    .set(values)
                    .execute(connection)?;
                let recent_seen = diesel::alias!(seen as recent_seen);
                diesel::delete(
                    seen::table.filter(seen::session.eq(&session)).filter(
                        seen::ordinal.ne_all(
                            recent_seen
                                .filter(recent_seen.field(seen::session).eq(&session))
                                .select(recent_seen.field(seen::ordinal))
                                .order(recent_seen.field(seen::ordinal).desc())
                                .limit(KEYS_PER_SESSION),
                        ),
                    ),
                )
                .execute(connection)?;
                let recent_notices = diesel::alias!(notices as recent_notices);
                diesel::delete(
                    notices::table.filter(
                        notices::session.ne_all(
                            recent_notices
                                .select(recent_notices.field(notices::session))
                                .order(recent_notices.field(notices::ordinal).desc())
                                .limit(MAX_SESSIONS),
                        ),
                    ),
                )
                .execute(connection)?;
                diesel::delete(
                    seen::table
                        .filter(seen::session.ne_all(notices::table.select(notices::session))),
                )
                .execute(connection)?;
                prune_owners(connection)?;
                Ok(Some(notice))
            });
        match result {
            Err(diesel::result::Error::RollbackTransaction) => Ok(None),
            other => other.map_err(|error| error.to_string()),
        }
    }

    pub fn list(&mut self) -> Result<Vec<Notice>, String> {
        notices::table
            .left_join(owners::table.on(owners::session.eq(notices::session)))
            .select((
                notices::id,
                notices::event,
                notices::delivered,
                notices::acknowledged,
                owners::window.nullable(),
            ))
            .order(notices::ordinal.desc())
            .load::<(String, String, bool, bool, Option<String>)>(&mut self.0)
            .map_err(|error| error.to_string())?
            .into_iter()
            .map(|(id, event, delivered, acknowledged, window)| {
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

    pub fn delivered(&mut self, id: Uuid) -> Result<(), String> {
        diesel::update(notices::table.filter(notices::id.eq(id.to_string())))
            .set(notices::delivered.eq(true))
            .execute(&mut self.0)
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    pub fn acknowledge(&mut self, id: Uuid) -> Result<Option<String>, String> {
        diesel::update(notices::table.filter(notices::id.eq(id.to_string())))
            .set((notices::acknowledged.eq(true), notices::delivered.eq(true)))
            .returning(notices::session)
            .get_result(&mut self.0)
            .optional()
            .map_err(|error| error.to_string())
    }

    pub fn retire(&mut self, target: Target) -> Result<(), String> {
        let key = target.storage_key();
        self.0
            .transaction::<_, diesel::result::Error, _>(|connection| {
                diesel::delete(notices::table.find(&key)).execute(connection)?;
                diesel::delete(seen::table.filter(seen::session.eq(&key))).execute(connection)?;
                diesel::delete(owners::table.find(&key)).execute(connection)?;
                Ok(())
            })
            .map_err(|error| error.to_string())
    }

    pub fn remember_window(
        &mut self,
        sessions: impl IntoIterator<Item = crate::model::TerminalId>,
        window: &str,
    ) -> Result<(), String> {
        self.0
            .transaction::<_, diesel::result::Error, _>(|connection| {
                for session in sessions {
                    remember_owner(connection, &session.to_string(), window)?;
                }
                prune_owners(connection)
            })
            .map_err(|error| error.to_string())
    }
}

fn remember_owner(
    connection: &mut SqliteConnection,
    session: &str,
    window: &str,
) -> QueryResult<()> {
    let ordinal = owners::table
        .select(diesel::dsl::max(owners::ordinal))
        .first::<Option<i64>>(connection)?
        .unwrap_or(0)
        + 1;
    let values = (owners::window.eq(window), owners::ordinal.eq(ordinal));
    diesel::insert_into(owners::table)
        .values((owners::session.eq(session), values))
        .on_conflict(owners::session)
        .do_update()
        .set(values)
        .execute(connection)
        .map(|_| ())
}

fn prune_owners(connection: &mut SqliteConnection) -> QueryResult<()> {
    let recent_owners = diesel::alias!(owners as recent_owners);
    diesel::delete(
        owners::table.filter(
            owners::session.ne_all(
                recent_owners
                    .select(recent_owners.field(owners::session))
                    .order(recent_owners.field(owners::ordinal).desc())
                    .limit(MAX_SESSIONS),
            ),
        ),
    )
    .execute(connection)
    .map(|_| ())
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        agent::Provider,
        model::TerminalId,
        notifications::{Evidence, Kind},
    };
    use rusqlite::Connection;

    fn seed_owners(journal: &mut Journal) {
        let rows: Vec<_> = (1..MAX_SESSIONS)
            .map(|i| {
                (
                    owners::session.eq(format!("old-{i}")),
                    owners::window.eq("other"),
                    owners::ordinal.eq(10000 + i),
                )
            })
            .collect();
        diesel::insert_into(owners::table)
            .values(rows)
            .execute(&mut journal.0)
            .unwrap();
    }

    #[test]
    fn failed_notice_write_rolls_back_replay_and_owner_changes() {
        let directory = tempfile::tempdir().unwrap();
        let mut journal = Journal::open(&directory.path().join("attention.sqlite3")).unwrap();
        let session = TerminalId::new();
        journal.remember_window([session], "owned").unwrap();
        journal.0.batch_execute("CREATE TRIGGER reject_notice BEFORE INSERT ON notices BEGIN SELECT RAISE(ABORT, 'injected write failure'); END;").unwrap();
        let mut run = event(session, "run");
        run.target = Target::AgentRun(crate::model::RunId::new());
        assert!(journal.record(run.clone(), false).is_err());
        assert_eq!(
            seen::table
                .count()
                .get_result::<i64>(&mut journal.0)
                .unwrap(),
            0
        );
        assert_eq!(
            owners::table
                .count()
                .get_result::<i64>(&mut journal.0)
                .unwrap(),
            1
        );
        journal
            .0
            .batch_execute("DROP TRIGGER reject_notice;")
            .unwrap();
        assert!(journal.record(run, false).unwrap().is_some());
    }

    #[cfg(unix)]
    #[test]
    fn database_uri_preserves_non_utf8_and_uri_metacharacters() {
        use std::os::unix::ffi::OsStringExt;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(std::ffi::OsString::from_vec(
            b"attention-\xff?#%.sqlite3".to_vec(),
        ));
        let mut journal = Journal::open(&path).unwrap();
        let notice = journal
            .record(event(TerminalId::new(), "uri"), false)
            .unwrap()
            .unwrap();
        drop(journal);
        assert_eq!(Journal::open(&path).unwrap().list().unwrap(), vec![notice]);
        assert!(path.is_file());
        assert!(
            !directory
                .path()
                .join("attention-\u{fffd}?#%.sqlite3")
                .exists()
        );
    }

    #[test]
    fn typed_schema_matches_existing_sqlite_columns_and_durability() {
        #[derive(diesel::QueryableByName)]
        struct Durability {
            #[diesel(sql_type = diesel::sql_types::Integer)]
            synchronous: i32,
        }
        #[derive(diesel::QueryableByName)]
        struct Timeout {
            #[diesel(sql_type = diesel::sql_types::Integer)]
            timeout: i32,
        }
        #[derive(diesel::QueryableByName)]
        struct Mode {
            #[diesel(sql_type = diesel::sql_types::Text)]
            journal_mode: String,
        }
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("attention.sqlite3");
        let mut journal = Journal::open(&path).unwrap();
        assert_eq!(
            diesel::sql_query("PRAGMA synchronous")
                .get_result::<Durability>(&mut journal.0)
                .unwrap()
                .synchronous,
            2
        );
        assert_eq!(
            diesel::sql_query("PRAGMA busy_timeout")
                .get_result::<Timeout>(&mut journal.0)
                .unwrap()
                .timeout,
            1000
        );
        assert_eq!(
            diesel::sql_query("PRAGMA journal_mode")
                .get_result::<Mode>(&mut journal.0)
                .unwrap()
                .journal_mode,
            "wal"
        );
        // Read through the old driver: no format conversion or new ledger.
        let legacy = Connection::open(&path).unwrap();
        for (table, expected) in [
            (
                "notices",
                vec![
                    ("session", "TEXT"),
                    ("id", "TEXT"),
                    ("event", "TEXT"),
                    ("delivered", "INTEGER"),
                    ("acknowledged", "INTEGER"),
                    ("ordinal", "INTEGER"),
                ],
            ),
            (
                "seen",
                vec![
                    ("session", "TEXT"),
                    ("evidence", "TEXT"),
                    ("ordinal", "INTEGER"),
                ],
            ),
            (
                "owners",
                vec![
                    ("session", "TEXT"),
                    ("window", "TEXT"),
                    ("ordinal", "INTEGER"),
                ],
            ),
            ("review_requests_seen", vec![("identity", "TEXT")]),
        ] {
            let actual: Vec<(String, String)> = legacy
                .prepare("SELECT name,type FROM pragma_table_info(?1) ORDER BY cid")
                .unwrap()
                .query_map([table], |row| Ok((row.get(0)?, row.get(1)?)))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap();
            assert_eq!(
                actual,
                expected
                    .into_iter()
                    .map(|(name, kind)| (name.to_owned(), kind.to_owned()))
                    .collect::<Vec<_>>()
            );
        }
        assert_eq!(
            legacy
                .pragma_query_value(None, "user_version", |row| row.get::<_, i32>(0))
                .unwrap(),
            3
        );
        let notice = journal
            .record(event(TerminalId::new(), "compatible"), false)
            .unwrap()
            .unwrap();
        assert_eq!(
            legacy
                .query_row("SELECT id FROM notices", [], |row| row.get::<_, String>(0))
                .unwrap(),
            notice.id.to_string()
        );
    }

    fn event(session: TerminalId, key: &str) -> Event {
        Event {
            target: crate::notifications::Target::Session(session),
            source_session: Some(session),
            provider: Some(Provider::Codex),
            directory: Some("/project".into()),
            destination: None,
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
    fn new_run_owner_uses_owner_recency_even_when_attention_is_infrequent() {
        let directory = tempfile::tempdir().unwrap();
        let mut journal = Journal::open(&directory.path().join("attention.sqlite3")).unwrap();
        let session = TerminalId::new();
        seed_owners(&mut journal);
        journal.remember_window([session], "chosen").unwrap();
        let mut run = event(session, "first-attention");
        run.target = Target::AgentRun(crate::model::RunId::new());
        journal.record(run, false).unwrap();
        assert_eq!(journal.list().unwrap()[0].window.as_deref(), Some("chosen"));
    }

    #[test]
    fn review_identity_survives_notice_eviction_restart_and_new_request() {
        use crate::notifications::{Target, review::ReviewRequest};
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("attention.sqlite3");
        let request = ReviewRequest {
            host: "github.com".into(),
            port: None,
            owner: "owner".into(),
            repository: "repo".into(),
            number: 51,
            viewer: "viewer".into(),
            request: "request-one".into(),
        };
        let make = |request: ReviewRequest| Event {
            evidence: Evidence {
                key: request.request.clone(),
                kind: Kind::ReviewRequested,
            },
            target: Target::PullRequest(request),
            source_session: None,
            provider: None,
            directory: None,
            destination: Some(crate::notifications::Destination {
                application: crate::notifications::Application::Ceditor,
                window: "review".into(),
            }),
        };
        let mut journal = Journal::open(&path).unwrap();
        let first = journal
            .record(make(request.clone()), false)
            .unwrap()
            .unwrap();
        journal.acknowledge(first.id).unwrap();
        // Notice payload compaction/removal is independent of this identity.
        journal
            .retire(Target::PullRequest(request.clone()))
            .unwrap();
        drop(journal);
        let mut journal = Journal::open(&path).unwrap();
        assert!(
            journal
                .record(make(request.clone()), false)
                .unwrap()
                .is_none()
        );
        let mut renamed = request.clone();
        renamed.repository = "renamed".into();
        assert!(journal.record(make(renamed), false).unwrap().is_none());
        let mut again = request;
        again.request = "request-two".into();
        assert!(journal.record(make(again), false).unwrap().is_some());
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
        diesel::update(notices::table)
            .set(notices::event.eq(legacy.to_string()))
            .execute(&mut journal.0)
            .unwrap();
        journal.0.batch_execute("PRAGMA user_version=1").unwrap();
        drop(journal);
        let mut journal = Journal::open(&path).unwrap();
        assert_eq!(journal.list().unwrap(), vec![notice]);
    }

    #[test]
    fn owner_recency_survives_infrequent_attention() {
        let directory = tempfile::tempdir().unwrap();
        let mut journal = Journal::open(&directory.path().join("attention.sqlite3")).unwrap();
        let session = TerminalId::new();
        seed_owners(&mut journal);
        journal.remember_window([session], "chosen").unwrap();
        journal
            .record(event(session, "first-attention"), false)
            .unwrap();
        journal
            .remember_window([TerminalId::new()], "other")
            .unwrap();
        assert_eq!(journal.list().unwrap()[0].window.as_deref(), Some("chosen"));
    }

    #[test]
    fn newer_journal_schema_is_rejected_without_schema_changes() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("attention.sqlite3");
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch("PRAGMA user_version=4; CREATE TABLE future(value TEXT);")
            .unwrap();
        drop(connection);
        assert!(Journal::open(&path).err().unwrap().contains("newer"));
        let connection = Connection::open(&path).unwrap();
        let version: u32 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 4);
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
        let count: i64 = seen::table.count().get_result(&mut journal.0).unwrap();
        assert_eq!(count, KEYS_PER_SESSION);
        assert_eq!(journal.list().unwrap().len(), 1);
        assert!(journal.list().unwrap()[0].delivered);
    }
}
