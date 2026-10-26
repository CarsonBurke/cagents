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
                    if matches!(event.target, Target::SessionInbox(_)) {
                        // Same arrival, potentially a successor terminal. Keep
                        // notice identity/dismissal/delivery; refresh routing
                        // metadata without publishing the backlog again.
                        diesel::update(notices::table.find(&session))
                            .set(notices::event.eq(&serialized))
                            .execute(connection)?;
                        if let Some(source) = event.source_session() {
                            if let Some(window) = owners::table
                                .find(source.to_string())
                                .select(owners::window)
                                .first::<String>(connection)
                                .optional()?
                            {
                                remember_owner(connection, &session, &window)?;
                            }
                        }
                        return Ok(None);
                    }
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

    /// Remove opt-in excerpts without changing attention, delivery or evidence
    /// identities. Turning privacy back on must also cover restored notices.
    pub fn clear_content(&mut self) -> Result<Vec<(Uuid, Target)>, String> {
        self.0
            .transaction::<_, diesel::result::Error, _>(|connection| {
                let rows = notices::table
                    .select((notices::session, notices::id, notices::event))
                    .load::<(String, String, String)>(connection)?;
                let mut changed = Vec::new();
                for (target, id, serialized) in rows {
                    let mut event = Event::from_journal(&serialized)
                        .map_err(|_| diesel::result::Error::RollbackTransaction)?;
                    if event.content.take().is_none() {
                        continue;
                    }
                    let serialized = serde_json::to_string(&event)
                        .map_err(|_| diesel::result::Error::RollbackTransaction)?;
                    let id = id
                        .parse()
                        .map_err(|_| diesel::result::Error::RollbackTransaction)?;
                    diesel::update(notices::table.find(target))
                        .set(notices::event.eq(serialized))
                        .execute(connection)?;
                    changed.push((id, event.target));
                }
                Ok(changed)
            })
            .map_err(|error| format!("Could not clear private notification previews: {error}"))
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

    #[test]
    fn disabling_previews_clears_retained_text_without_changing_acknowledgement() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("notifications.sqlite3");
        let mut journal = Journal::open(&path).unwrap();
        let mut original = event(TerminalId::new(), "one");
        original.content = Some(crate::notifications::Content {
            conversation: "Private conversation".into(),
            preview: "Private response".into(),
            request: None,
        });
        let notice = journal.record(original, false).unwrap().unwrap();
        journal.delivered(notice.id).unwrap();
        journal.acknowledge(notice.id).unwrap();
        let cleared = journal.clear_content().unwrap();
        assert_eq!(cleared.len(), 1);
        assert_eq!(cleared[0].0, notice.id);
        assert_eq!(cleared[0].1, notice.event.target);
        assert!(journal.clear_content().unwrap().is_empty());
        drop(journal);
        let mut reopened = Journal::open(&path).unwrap();
        let retained = reopened.list().unwrap();
        assert_eq!(retained[0].id, notice.id);
        assert!(retained[0].acknowledged && retained[0].delivered);
        assert!(retained[0].event.content.is_none());
    }

    #[test]
    fn acknowledgement_of_first_question_cannot_swallow_the_next_waiting_request() {
        let directory = tempfile::tempdir().unwrap();
        let mut journal = Journal::open(&directory.path().join("notifications.sqlite3")).unwrap();
        let session = TerminalId::new();
        let mut first = event(session, "turn:waiting:1");
        first.evidence.kind = Kind::Waiting;
        let first = journal.record(first, false).unwrap().unwrap();
        let mut second = event(session, "turn:waiting:2");
        second.evidence.kind = Kind::Waiting;
        let second = journal.record(second, false).unwrap().unwrap();
        assert_ne!(first.id, second.id);
        assert!(journal.acknowledge(first.id).unwrap().is_none());
        let notices = journal.list().unwrap();
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].id, second.id);
        assert!(!notices[0].acknowledged);
    }

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

    fn inbox_event(
        lineage: crate::agent_execution::Lineage,
        session: TerminalId,
        message: crate::model::MessageId,
    ) -> Event {
        Event {
            content: None,
            target: Target::SessionInbox(lineage),
            source_session: Some(session),
            provider: None,
            directory: None,
            destination: None,
            evidence: Evidence {
                key: message.to_string(),
                kind: Kind::MessageReceived,
            },
        }
    }

    #[test]
    fn a_reply_read_during_its_hold_never_replaces_dismissed_older_attention() {
        use crate::agent_execution::{
            Lineage,
            mailbox::{Message, Party, Receipt, State},
        };
        let directory = tempfile::tempdir().unwrap();
        let mut registry =
            crate::host::model::Registry::at(&directory.path().join("model.sqlite3")).unwrap();
        let mut journal = Journal::open(&directory.path().join("attention.sqlite3")).unwrap();
        let lineage = Lineage(TerminalId::new());
        let other = Lineage(TerminalId::new());
        let party = |lineage: Lineage| Party {
            lineage: Some(lineage),
            session: Some(lineage.0),
            title: None,
        };
        let message = |from, to, created_ms| Message {
            id: crate::model::MessageId::new(),
            from,
            to,
            reply_to: None,
            body: "private message".into(),
            wait_until_ms: None,
            state: State::Pending,
            created_ms,
        };
        let older = message(Party::user(), party(lineage), 1000);
        registry.post_message(&older).unwrap();
        let notice = journal
            .record(
                inbox_event(
                    lineage,
                    lineage.0,
                    registry.reconcile_message_attention(50).unwrap()[&lineage],
                ),
                false,
            )
            .unwrap()
            .unwrap();
        journal.acknowledge(notice.id).unwrap();
        let mut ask = message(party(lineage), party(other), 1);
        ask.wait_until_ms = Some(100);
        registry.post_message(&ask).unwrap();
        let mut held = message(party(other), party(lineage), 3);
        held.reply_to = Some(ask.id);
        registry.post_message(&held).unwrap();
        // The real ask path peeks replies and acknowledges a receipt during
        // their hold, independently of the older unread inbox message.
        registry
            .acknowledge_messages(Some(lineage), None, &Receipt { ids: vec![held.id] }, 50)
            .unwrap();
        for now in [50, 100] {
            let head = registry.reconcile_message_attention(now).unwrap()[&lineage];
            assert_eq!(head, older.id);
            assert!(
                journal
                    .record(inbox_event(lineage, lineage.0, head), false)
                    .unwrap()
                    .is_none()
            );
        }
        // Expiring the original ask cannot turn its already-read reply into
        // an arrival either; the eligible head remains durable.
        registry.prune_messages(2).unwrap();
        assert_eq!(
            registry.reconcile_message_attention(200).unwrap()[&lineage],
            older.id
        );
        let current = journal.list().unwrap().pop().unwrap();
        assert_eq!(current.id, notice.id);
        assert!(current.acknowledged && current.delivered);
    }

    #[test]
    fn held_reply_release_does_not_reannounce_a_dismissed_later_arrival() {
        use crate::agent_execution::{
            Lineage,
            mailbox::{Message, Party, Receipt, State},
        };
        let directory = tempfile::tempdir().unwrap();
        let mut registry =
            crate::host::model::Registry::at(&directory.path().join("model.sqlite3")).unwrap();
        let mut journal = Journal::open(&directory.path().join("attention.sqlite3")).unwrap();
        let lineage = Lineage(TerminalId::new());
        let other = Lineage(TerminalId::new());
        let party = |lineage: Lineage| Party {
            lineage: Some(lineage),
            session: Some(lineage.0),
            title: None,
        };
        let message = |from, to| Message {
            id: crate::model::MessageId::new(),
            from,
            to,
            reply_to: None,
            body: "private message".into(),
            wait_until_ms: None,
            state: State::Pending,
            created_ms: 1,
        };
        let mut ask = message(party(lineage), party(other));
        ask.wait_until_ms = Some(100);
        registry.post_message(&ask).unwrap();
        let mut held = message(party(other), party(lineage));
        held.reply_to = Some(ask.id);
        registry.post_message(&held).unwrap();
        assert!(
            !registry
                .reconcile_message_attention(50)
                .unwrap()
                .contains_key(&lineage)
        );
        let ordinary = message(Party::user(), party(lineage));
        registry.post_message(&ordinary).unwrap();
        let notice = journal
            .record(
                inbox_event(
                    lineage,
                    lineage.0,
                    registry.reconcile_message_attention(50).unwrap()[&lineage],
                ),
                false,
            )
            .unwrap()
            .unwrap();
        journal.acknowledge(notice.id).unwrap();
        // Dismissing the alert does not acknowledge either message.
        assert!(
            registry
                .pending_message_lineages()
                .unwrap()
                .contains(&lineage)
        );
        registry
            .acknowledge_messages(
                Some(lineage),
                None,
                &Receipt {
                    ids: vec![ordinary.id],
                },
                50,
            )
            .unwrap();
        assert!(
            !registry
                .reconcile_message_attention(50)
                .unwrap()
                .contains_key(&lineage)
        );
        assert!(
            journal
                .record(
                    inbox_event(
                        lineage,
                        lineage.0,
                        registry.reconcile_message_attention(100).unwrap()[&lineage]
                    ),
                    false
                )
                .unwrap()
                .is_none()
        );
        let current = journal.list().unwrap().pop().unwrap();
        assert_eq!(current.id, notice.id);
        assert!(current.acknowledged && current.delivered);
    }

    #[test]
    fn inbox_dismissal_survives_restart_and_successor_but_new_arrival_reactivates() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("attention.sqlite3");
        let mut journal = Journal::open(&path).unwrap();
        let session = TerminalId::new();
        let lineage = crate::agent_execution::Lineage(session);
        let message = crate::model::MessageId::new();
        let original = journal
            .record(inbox_event(lineage, session, message), false)
            .unwrap()
            .unwrap();
        journal.acknowledge(original.id).unwrap();
        let lifecycle = journal
            .record(event(session, "finished"), false)
            .unwrap()
            .unwrap();
        drop(journal);
        let mut journal = Journal::open(&path).unwrap();
        let successor = TerminalId::new();
        journal.remember_window([successor], "successor").unwrap();
        assert!(
            journal
                .record(inbox_event(lineage, successor, message), false)
                .unwrap()
                .is_none()
        );
        let inbox = journal
            .list()
            .unwrap()
            .into_iter()
            .find(|notice| notice.event.target == Target::SessionInbox(lineage))
            .unwrap();
        assert_eq!(inbox.id, original.id);
        assert!(inbox.acknowledged && inbox.delivered);
        assert_eq!(inbox.event.source_session(), Some(successor));
        assert_eq!(inbox.window.as_deref(), Some("successor"));
        let next = journal
            .record(
                inbox_event(lineage, successor, crate::model::MessageId::new()),
                false,
            )
            .unwrap()
            .unwrap();
        assert_ne!(next.id, original.id);
        assert!(!next.acknowledged && !next.delivered);
        journal.retire(Target::SessionInbox(lineage)).unwrap();
        assert_eq!(journal.list().unwrap()[0].id, lifecycle.id);
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
            content: None,
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
            content: None,
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

    #[test]
    fn automation_lifecycle_replay_keeps_latest_notice_and_actor_retirement_separate() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("automation.sqlite3");
        let id = Uuid::new_v4();
        let make = |sequence, kind| Event {
            target: Target::Automation { id, sequence },
            source_session: None,
            provider: None,
            directory: None,
            destination: None,
            evidence: Evidence {
                key: format!("automation:{id}:lifecycle:{sequence}"),
                kind,
            },
            content: None,
        };
        let old = make(17, Kind::AutomationPaused);
        let current = make(18, Kind::AutomationBlocked);
        let mut journal = Journal::open(&path).unwrap();
        let first = journal.record(old.clone(), false).unwrap().unwrap();
        journal.acknowledge(first.id).unwrap();
        let second = journal.record(current.clone(), false).unwrap().unwrap();
        assert_ne!(first.id, second.id);
        journal
            .retire(Target::Session(TerminalId::from_uuid(id)))
            .unwrap();
        journal
            .retire(Target::AgentRun(crate::model::RunId::from_uuid(id)))
            .unwrap();
        assert_eq!(
            journal.list().unwrap(),
            vec![second.clone()],
            "actor predecessor retirement cannot own automation history"
        );
        journal.acknowledge(first.id).unwrap();
        assert!(!journal.list().unwrap()[0].acknowledged);
        drop(journal);
        let mut reopened = Journal::open(&path).unwrap();
        assert!(
            reopened.record(old, false).unwrap().is_none(),
            "older durable duplicate cannot restore its earlier target or kind"
        );
        assert_eq!(reopened.list().unwrap(), vec![second.clone()]);
        reopened.acknowledge(second.id).unwrap();
        assert!(reopened.record(current, false).unwrap().is_none());
        let final_notice = reopened.list().unwrap().pop().unwrap();
        assert_eq!(final_notice.id, second.id);
        assert!(final_notice.acknowledged);
    }
}
