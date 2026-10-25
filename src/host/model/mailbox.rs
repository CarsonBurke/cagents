//! Messages left for a session's lineage, and the key that seals them.
//! A message is pending until the courier gives it to a live conversation or
//! its recipient reads it; either settles it once.

use super::Registry;
use crate::{
    agent_execution::{
        Lineage,
        mailbox::{Inbox, MAX_PENDING, Message, Party, Receipt, State},
    },
    model::{MessageId, TerminalId},
};
use rusqlite::{
    Connection, Row, TransactionBehavior, params,
    types::{ToSql, ToSqlOutput},
};

/// How many messages `cagents inbox --all` lists.
const RECENT: usize = 50;

pub(super) fn migrate(tx: &rusqlite::Transaction<'_>) -> Result<(), String> {
    let current: u32 = tx
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(|error| error.to_string())?;
    if current < 5 {
        // SQLite draws `randomblob` from the operating system's entropy.
        tx.execute_batch(
            "CREATE TABLE IF NOT EXISTS mailbox_key(
                 key INTEGER PRIMARY KEY CHECK(key = 0),
                 secret BLOB NOT NULL CHECK(length(secret) = 32)
             ) STRICT;
             INSERT OR IGNORE INTO mailbox_key(key, secret) VALUES (0, randomblob(32));
             CREATE TABLE IF NOT EXISTS messages(
                 id TEXT PRIMARY KEY,
                 sender_lineage TEXT,
                 sender_session TEXT,
                 sender_title TEXT,
                 recipient_lineage TEXT,
                 delivered_session TEXT,
                 reply_to TEXT,
                 body TEXT NOT NULL,
                 wait_until_ms INTEGER,
                 state TEXT NOT NULL CHECK(state IN ('pending', 'delivered', 'read')),
                 created_ms INTEGER NOT NULL,
                 settled_ms INTEGER
             ) STRICT;
             CREATE INDEX IF NOT EXISTS messages_by_recipient
                 ON messages(recipient_lineage, state, created_ms);
             CREATE INDEX IF NOT EXISTS messages_pending
                 ON messages(created_ms) WHERE state = 'pending';
             CREATE INDEX IF NOT EXISTS messages_by_reply ON messages(reply_to);",
        )
        .map_err(|error| error.to_string())?;
    }
    Ok(())
}

/// Eligible arrival heads are notification evidence, not read receipts.
/// Normal posts advance atomically; held replies are admitted only while
/// still pending after release. Partial reads never manufacture an arrival.
pub(super) fn migrate_attention(tx: &rusqlite::Transaction<'_>) -> Result<(), String> {
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS mailbox_attention_heads(
        lineage TEXT PRIMARY KEY, message TEXT NOT NULL
    ) STRICT;
    INSERT OR IGNORE INTO mailbox_attention_heads(lineage,message)
        SELECT recipient_lineage,id FROM messages WHERE rowid IN (
            SELECT MAX(m.rowid) FROM messages m LEFT JOIN messages asked ON asked.id=m.reply_to
            WHERE m.recipient_lineage IS NOT NULL
              AND (m.reply_to IS NULL
                   OR (asked.id IS NOT NULL AND (asked.wait_until_ms IS NULL
                       OR asked.wait_until_ms <= m.created_ms
                       OR (m.state != 'pending' AND m.settled_ms >= asked.wait_until_ms)))
                   OR (asked.id IS NULL AND m.state='pending'))
            GROUP BY m.recipient_lineage
        );",
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// A lineage column; NULL stands for the user.
struct Recipient(Option<Lineage>);

impl ToSql for Recipient {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(match self.0 {
            Some(lineage) => ToSqlOutput::from(lineage.to_string()),
            None => ToSqlOutput::from(rusqlite::types::Null),
        })
    }
}

const COLUMNS: &str = "id, sender_lineage, sender_session, sender_title, recipient_lineage,
     delivered_session, reply_to, body, wait_until_ms, state, created_ms";

fn message(row: &Row<'_>) -> rusqlite::Result<Message> {
    fn id<T: std::str::FromStr>(row: &Row<'_>, column: usize) -> rusqlite::Result<Option<T>> {
        row.get::<_, Option<String>>(column)?
            .map(|value| {
                value.parse().map_err(|_| {
                    rusqlite::Error::FromSqlConversionFailure(
                        column,
                        rusqlite::types::Type::Text,
                        format!("stored message ID `{value}` is invalid").into(),
                    )
                })
            })
            .transpose()
    }
    Ok(Message {
        id: id(row, 0)?.ok_or(rusqlite::Error::InvalidColumnType(
            0,
            "id".into(),
            rusqlite::types::Type::Null,
        ))?,
        from: Party {
            lineage: id::<TerminalId>(row, 1)?.map(Lineage),
            session: id(row, 2)?,
            title: row.get(3)?,
        },
        to: Party {
            lineage: id::<TerminalId>(row, 4)?.map(Lineage),
            session: id(row, 5)?,
            title: None,
        },
        reply_to: id(row, 6)?,
        body: row.get(7)?,
        wait_until_ms: row.get::<_, Option<i64>>(8)?.map(|ms| ms as u64),
        state: match row.get::<_, String>(9)?.as_str() {
            "pending" => State::Pending,
            "delivered" => State::Delivered,
            _ => State::Read,
        },
        created_ms: row.get::<_, i64>(10)? as u64,
    })
}

// Leave room for the correlated server envelope and the authenticated
// recipient session added to pending copies after selection. The body and
// metadata are measured together, so large historical titles also fit safely.
const INBOX_MESSAGE_BYTES: usize = crate::terminal::ipc::MAX_FRAME_BYTES - 4096;

fn select_messages(
    db: &Connection,
    recipient: Option<Lineage>,
    query: &Inbox,
) -> Result<Vec<Message>, String> {
    let recent = query.all && query.replies_to.is_none();
    let (filter, limit) = match (&query.replies_to, query.all) {
        (Some(_), _) => (
            "AND reply_to = ?2 AND state = 'pending'",
            MAX_PENDING as i64,
        ),
        (None, true) => ("AND ?2 IS NULL", RECENT as i64),
        (None, false) => ("AND ?2 IS NULL AND state = 'pending'", MAX_PENDING as i64),
    };
    // Pending batches start with the oldest records so new arrivals cannot
    // displace messages between reads. History still selects the newest first.
    let order = if recent { "DESC" } else { "ASC" };
    let mut statement = db
        .prepare_cached(&format!(
            "SELECT {COLUMNS} FROM messages WHERE recipient_lineage IS ?1 {filter}
             ORDER BY created_ms {order}, rowid {order} LIMIT ?3"
        ))
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map(
            params![
                Recipient(recipient),
                query.replies_to.map(|id| id.to_string()),
                limit
            ],
            message,
        )
        .map_err(|error| error.to_string())?;
    let mut messages = Vec::new();
    let mut bytes = 0;
    for row in rows {
        let message = row.map_err(|error| error.to_string())?;
        let size =
            postcard::experimental::serialized_size(&message).map_err(|error| error.to_string())?;
        if size > INBOX_MESSAGE_BYTES - bytes {
            if messages.is_empty() {
                return Err(format!(
                    "Message {} cannot fit in an inbox reply; its state is unchanged",
                    message.id
                ));
            }
            break;
        }
        bytes += size;
        messages.push(message);
    }
    if recent {
        messages.reverse();
    }
    Ok(messages)
}

impl Registry {
    /// The key that seals delivered messages, made when the store was.
    pub(crate) fn mailbox_key(&self) -> Result<[u8; 32], String> {
        let secret: Vec<u8> = self
            .db
            .query_row("SELECT secret FROM mailbox_key WHERE key = 0", [], |row| {
                row.get(0)
            })
            .map_err(|error| error.to_string())?;
        secret
            .try_into()
            .map_err(|_| "The stored mailbox key is invalid".to_owned())
    }

    /// Records a new message, unless its recipient already has as many
    /// waiting as it may. The user reads messages only when they choose to,
    /// so each sender has its own allowance there and one cannot use up
    /// everyone's.
    pub(crate) fn post_message(&mut self, message: &Message) -> Result<(), String> {
        let tx = self.db.transaction().map_err(|error| error.to_string())?;
        let pending: i64 = tx
            .query_row(
                "SELECT COUNT(*) FROM messages
                 WHERE recipient_lineage IS ?1 AND state = 'pending'
                   AND (?1 IS NOT NULL OR sender_lineage IS ?2)",
                params![
                    Recipient(message.to.lineage),
                    Recipient(message.from.lineage)
                ],
                |row| row.get(0),
            )
            .map_err(|error| error.to_string())?;
        if pending as usize >= MAX_PENDING {
            return Err(match message.to.lineage {
                Some(_) => format!(
                    "{} already has {MAX_PENDING} messages waiting; try again once it has read them",
                    message.to.label()
                ),
                None => format!(
                    "The user has not read your last {MAX_PENDING} messages; wait for them to"
                ),
            });
        }
        tx.execute(
            "INSERT INTO messages(id, sender_lineage, sender_session, sender_title,
                 recipient_lineage, reply_to, body, wait_until_ms, state, created_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'pending', ?9)",
            params![
                message.id.to_string(),
                Recipient(message.from.lineage),
                message.from.session.map(|id| id.to_string()),
                message.from.title,
                Recipient(message.to.lineage),
                message.reply_to.map(|id| id.to_string()),
                message.body,
                message.wait_until_ms.map(|ms| ms as i64),
                message.created_ms as i64,
            ],
        )
        .map_err(|error| error.to_string())?;
        if let Some(lineage) = message.to.lineage {
            tx.execute(
                "INSERT INTO mailbox_attention_heads(lineage,message)
                SELECT ?1,?2 WHERE NOT EXISTS(SELECT 1 FROM messages asked
                    WHERE asked.id=?3 AND asked.wait_until_ms > ?4)
                ON CONFLICT(lineage) DO UPDATE SET message=excluded.message",
                params![
                    lineage.to_string(),
                    message.id.to_string(),
                    message.reply_to.map(|id| id.to_string()),
                    message.created_ms as i64
                ],
            )
            .map_err(|e| e.to_string())?;
        }
        tx.commit().map_err(|error| error.to_string())
    }

    /// Actual unread recipients, including temporarily held replies. This
    /// controls retirement independently from arrival eligibility.
    pub(crate) fn pending_message_lineages(
        &self,
    ) -> Result<std::collections::BTreeSet<Lineage>, String> {
        let mut query = self.db.prepare_cached("SELECT DISTINCT recipient_lineage FROM messages WHERE state='pending' AND recipient_lineage IS NOT NULL").map_err(|e| e.to_string())?;
        query
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|e| e.to_string())?
            .map(|row| {
                Ok(Lineage(
                    row.map_err(|e| e.to_string())?
                        .parse()
                        .map_err(|e: uuid::Error| e.to_string())?,
                ))
            })
            .collect()
    }

    /// Admit newly eligible pending arrivals under the caller's existing
    /// registry lock. A reply read during its hold is never admitted later.
    /// Newer ordinary arrival heads cover older held replies on release.
    pub(crate) fn reconcile_message_attention(
        &mut self,
        now: u64,
    ) -> Result<std::collections::BTreeMap<Lineage, MessageId>, String> {
        self.db
            .execute(
                "INSERT INTO mailbox_attention_heads(lineage,message)
            SELECT recipient_lineage,id FROM messages WHERE rowid IN (
                SELECT MAX(p.rowid) FROM messages p LEFT JOIN messages asked ON asked.id=p.reply_to
                WHERE p.recipient_lineage IS NOT NULL AND p.state='pending'
                  AND (asked.wait_until_ms IS NULL OR asked.wait_until_ms <= ?1)
                GROUP BY p.recipient_lineage
            )
            ON CONFLICT(lineage) DO UPDATE SET message=excluded.message
            WHERE (SELECT rowid FROM messages WHERE id=excluded.message) >
                COALESCE((SELECT rowid FROM messages WHERE id=mailbox_attention_heads.message),0)",
                [now as i64],
            )
            .map_err(|e| e.to_string())?;
        let mut query = self.db.prepare_cached("SELECT h.lineage,h.message FROM mailbox_attention_heads h
            WHERE EXISTS(SELECT 1 FROM messages pending LEFT JOIN messages asked ON asked.id=pending.reply_to
                WHERE pending.recipient_lineage=h.lineage AND pending.state='pending'
                  AND (asked.wait_until_ms IS NULL OR asked.wait_until_ms <= ?1))").map_err(|e| e.to_string())?;
        query
            .query_map([now as i64], |row| {
                let lineage: String = row.get(0)?;
                let message: String = row.get(1)?;
                Ok((lineage, message))
            })
            .map_err(|e| e.to_string())?
            .map(|row| {
                let (lineage, message) = row.map_err(|e| e.to_string())?;
                Ok((
                    Lineage(lineage.parse().map_err(|e: uuid::Error| e.to_string())?),
                    message.parse().map_err(|e: uuid::Error| e.to_string())?,
                ))
            })
            .collect()
    }

    /// The message `recipient` received whose ID begins with `handle`.
    pub(crate) fn received_message(
        &self,
        recipient: Option<Lineage>,
        handle: &str,
    ) -> Result<Message, String> {
        let handle = handle.to_ascii_lowercase();
        if !handle.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
            return Err(format!("No message {handle} was left for you"));
        }
        let mut statement = self
            .db
            .prepare_cached(&format!(
                "SELECT {COLUMNS} FROM messages
                 WHERE recipient_lineage IS ?1 AND substr(id, 1, length(?2)) = ?2
                 LIMIT 2"
            ))
            .map_err(|error| error.to_string())?;
        let found: Vec<Message> = statement
            .query_map(params![Recipient(recipient), handle], message)
            .and_then(Iterator::collect)
            .map_err(|error| error.to_string())?;
        match <[Message; 1]>::try_from(found) {
            Ok([message]) => Ok(message),
            Err(found) if found.is_empty() => Err(format!("No message {handle} was left for you")),
            Err(_) => Err(format!(
                "Several messages begin with {handle}; give more of the ID"
            )),
        }
    }

    /// The oldest pending message each lineage may now receive, and when the
    /// next reply held for a waiting asker is released. One at a time: the
    /// next waits for the turn this one starts to finish. A message to the
    /// user is never delivered: the user reads it.
    pub(crate) fn deliverable_messages(
        &self,
        now: u64,
    ) -> Result<(Vec<Message>, Option<u64>), String> {
        let mut statement = self
            .db
            .prepare_cached(&format!(
                "SELECT {COLUMNS} FROM messages WHERE rowid IN (
                     SELECT message_rowid FROM (
                         SELECT m.rowid AS message_rowid, row_number() OVER (
                             PARTITION BY m.recipient_lineage ORDER BY m.created_ms, m.rowid
                         ) AS position
                         FROM messages m
                         LEFT JOIN messages asked ON asked.id = m.reply_to
                         WHERE m.state = 'pending' AND m.recipient_lineage IS NOT NULL
                           AND (asked.wait_until_ms IS NULL OR asked.wait_until_ms <= ?1)
                     ) WHERE position = 1
                 )
                 ORDER BY created_ms, recipient_lineage"
            ))
            .map_err(|error| error.to_string())?;
        let messages = statement
            .query_map([now as i64], message)
            .and_then(Iterator::collect)
            .map_err(|error| error.to_string())?;
        let released: Option<i64> = self
            .db
            .query_row(
                "SELECT MIN(asked.wait_until_ms) FROM messages m
                 JOIN messages asked ON asked.id = m.reply_to
                 WHERE m.state = 'pending' AND asked.wait_until_ms > ?1",
                [now as i64],
                |row| row.get(0),
            )
            .map_err(|error| error.to_string())?;
        Ok((messages, released.map(|ms| ms as u64)))
    }

    /// Settles a message as given to `session`. False when it was already
    /// settled, by another delivery or by its recipient reading it.
    pub(crate) fn deliver_message(
        &mut self,
        id: MessageId,
        session: TerminalId,
        now: u64,
    ) -> Result<bool, String> {
        self.db
            .execute(
                "UPDATE messages SET state = 'delivered', delivered_session = ?2, settled_ms = ?3
                 WHERE id = ?1 AND state = 'pending'",
                params![id.to_string(), session.to_string(), now as i64],
            )
            .map(|changed| changed == 1)
            .map_err(|error| error.to_string())
    }

    /// Returns a message `session` refused to the queue it was claimed from.
    pub(crate) fn undeliver_message(
        &mut self,
        id: MessageId,
        session: TerminalId,
    ) -> Result<(), String> {
        self.db
            .execute(
                "UPDATE messages SET state = 'pending', delivered_session = NULL, settled_ms = NULL
                 WHERE id = ?1 AND state = 'delivered' AND delivered_session = ?2",
                params![id.to_string(), session.to_string()],
            )
            .map(drop)
            .map_err(|error| error.to_string())
    }

    /// Returns bodies without changing delivery state. A reply that cannot
    /// reach its client therefore leaves the messages available to retry.
    pub(crate) fn peek_messages(
        &self,
        recipient: Option<Lineage>,
        query: &Inbox,
    ) -> Result<Vec<Message>, String> {
        select_messages(&self.db, recipient, query)
    }

    /// Atomically validates the whole receipt against its recipient before
    /// settling pending messages. Already settled or pruned IDs are harmless.
    pub(crate) fn acknowledge_messages(
        &mut self,
        recipient: Option<Lineage>,
        session: Option<TerminalId>,
        receipt: &Receipt,
        now: u64,
    ) -> Result<(), String> {
        receipt.validate()?;
        let tx = self.db.transaction().map_err(|error| error.to_string())?;
        for id in &receipt.ids {
            let foreign: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM messages WHERE id = ?1 AND recipient_lineage IS NOT ?2)",
                params![id.to_string(), Recipient(recipient)],
                |row| row.get(0),
            ).map_err(|error| error.to_string())?;
            if foreign {
                return Err("A mailbox receipt may acknowledge only the caller's messages".into());
            }
        }
        for id in &receipt.ids {
            tx.execute(
                "UPDATE messages SET state = 'read', delivered_session = ?2, settled_ms = ?3
                 WHERE id = ?1 AND state = 'pending' AND recipient_lineage IS ?4",
                params![
                    id.to_string(),
                    session.map(|id| id.to_string()),
                    now as i64,
                    Recipient(recipient)
                ],
            )
            .map_err(|error| error.to_string())?;
        }
        tx.commit().map_err(|error| error.to_string())
    }

    /// The messages `recipient` asks for, oldest first. Those it had not yet
    /// received are marked read, so the courier no longer delivers them.
    pub(crate) fn read_messages(
        &mut self,
        recipient: Option<Lineage>,
        session: Option<TerminalId>,
        query: &Inbox,
        now: u64,
    ) -> Result<Vec<Message>, String> {
        let tx = self.db.transaction().map_err(|error| error.to_string())?;
        let mut messages = select_messages(&tx, recipient, query)?;
        for message in &mut messages {
            if message.state != State::Pending {
                continue;
            }
            tx.execute(
                "UPDATE messages SET state = 'read', delivered_session = ?2, settled_ms = ?3
                 WHERE id = ?1 AND state = 'pending'",
                params![
                    message.id.to_string(),
                    session.map(|id| id.to_string()),
                    now as i64
                ],
            )
            .map_err(|error| error.to_string())?;
            message.state = State::Read;
            message.to.session = session;
        }
        tx.commit().map_err(|error| error.to_string())?;
        Ok(messages)
    }

    /// Forgets messages sent before `before`, whatever became of them.
    pub(crate) fn prune_messages(&mut self, before: u64) -> Result<usize, String> {
        let tx = self.db.transaction().map_err(|e| e.to_string())?;
        let removed = tx
            .execute(
                "DELETE FROM messages WHERE created_ms < ?1",
                [before as i64],
            )
            .map_err(|e| e.to_string())?;
        tx.execute_batch(
            "DELETE FROM mailbox_attention_heads WHERE NOT EXISTS(
                SELECT 1 FROM messages WHERE id=mailbox_attention_heads.message);",
        )
        .map_err(|e| e.to_string())?;
        tx.commit().map_err(|e| e.to_string())?;
        Ok(removed)
    }

    #[cfg(test)]
    pub(crate) fn message(&self, id: MessageId) -> Result<Option<Message>, String> {
        use rusqlite::OptionalExtension;
        self.db
            .query_row(
                &format!("SELECT {COLUMNS} FROM messages WHERE id = ?1"),
                [id.to_string()],
                message,
            )
            .optional()
            .map_err(|error| error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> (Registry, tempfile::TempDir) {
        let directory = tempfile::tempdir().unwrap();
        let registry = Registry::at(&directory.path().join("model.sqlite3")).unwrap();
        (registry, directory)
    }

    fn party(lineage: Lineage) -> Party {
        Party {
            lineage: Some(lineage),
            session: Some(lineage.0),
            title: None,
        }
    }

    fn message(from: Party, to: Party, created_ms: u64) -> Message {
        Message {
            id: MessageId::new(),
            from,
            to,
            reply_to: None,
            body: "Hold off on src/auth.rs".into(),
            wait_until_ms: None,
            state: State::Pending,
            created_ms,
        }
    }

    fn assert_inbox_fits_wire(messages: &[Message]) {
        use crate::{
            model::HostSessionProof,
            terminal::ipc::{ServerEnvelope, ServerMessage, ServerResponse},
        };
        let session = TerminalId::new();
        let projected: Vec<_> = messages
            .iter()
            .cloned()
            .map(|mut entry| {
                if entry.state == State::Pending {
                    entry.to.session = Some(session);
                }
                entry
            })
            .collect();
        let envelope = ServerEnvelope {
            version: crate::terminal::ipc::PROTOCOL_VERSION,
            request_id: Some(u64::MAX),
            proof: HostSessionProof {
                host_session_id: crate::model::HostSessionId::new(uuid::Uuid::new_v4().to_string()),
                boot_id: crate::model::BootId::new(uuid::Uuid::new_v4().to_string()),
                host_incarnation: crate::model::HostIncarnation::new(
                    uuid::Uuid::new_v4().to_string(),
                ),
            },
            message: ServerMessage::Response {
                response: ServerResponse::Agent(crate::agent_execution::Reply::Inbox(projected)),
            },
        };
        crate::terminal::ipc::write_frame(&mut Vec::new(), &envelope).unwrap();
    }

    #[test]
    fn pruning_an_eligible_head_cannot_promote_a_prematurely_read_held_reply() {
        let (mut registry, _directory) = registry();
        let recipient = Lineage(TerminalId::new());
        let other = Lineage(TerminalId::new());
        let retained = message(Party::user(), party(recipient), 1000);
        let expired_head = message(Party::user(), party(recipient), 1);
        registry.post_message(&retained).unwrap();
        registry.post_message(&expired_head).unwrap();
        let mut ask = message(party(recipient), party(other), 3);
        ask.wait_until_ms = Some(100);
        registry.post_message(&ask).unwrap();
        let mut held = message(party(other), party(recipient), 4);
        held.reply_to = Some(ask.id);
        registry.post_message(&held).unwrap();
        registry
            .acknowledge_messages(Some(recipient), None, &Receipt { ids: vec![held.id] }, 50)
            .unwrap();
        registry.prune_messages(2).unwrap();
        for now in [50, 100] {
            assert_eq!(
                registry.reconcile_message_attention(now).unwrap()[&recipient],
                retained.id
            );
        }
    }

    #[test]
    fn migration_keeps_eligible_head_instead_of_a_prematurely_read_held_reply() {
        let (mut registry, _directory) = registry();
        let recipient = Lineage(TerminalId::new());
        let other = Lineage(TerminalId::new());
        let older = message(Party::user(), party(recipient), 1);
        registry.post_message(&older).unwrap();
        let mut ask = message(party(recipient), party(other), 2);
        ask.wait_until_ms = Some(100);
        registry.post_message(&ask).unwrap();
        let mut held = message(party(other), party(recipient), 3);
        held.reply_to = Some(ask.id);
        registry.post_message(&held).unwrap();
        registry
            .acknowledge_messages(Some(recipient), None, &Receipt { ids: vec![held.id] }, 50)
            .unwrap();
        registry
            .db
            .execute_batch(
                "DROP TABLE channel_work_operations;
                DROP TABLE channel_work_entries;
                DROP TABLE channel_work_homes;
                DROP TABLE mailbox_attention_heads; PRAGMA user_version=11;",
            )
            .unwrap();
        let tx = registry
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        migrate_attention(&tx).unwrap();
        tx.pragma_update(None, "user_version", 12).unwrap();
        tx.commit().unwrap();
        assert_eq!(
            registry.reconcile_message_attention(200).unwrap()[&recipient],
            older.id
        );
        // A legacy store may already have expired the original ask. Missing
        // eligibility proof cannot promote an already-settled reply.
        registry
            .db
            .execute("DELETE FROM messages WHERE id=?1", [ask.id.to_string()])
            .unwrap();
        registry
            .db
            .execute_batch("DROP TABLE mailbox_attention_heads; PRAGMA user_version=11;")
            .unwrap();
        let tx = registry
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        migrate_attention(&tx).unwrap();
        tx.pragma_update(None, "user_version", 12).unwrap();
        tx.commit().unwrap();
        assert_eq!(
            registry.reconcile_message_attention(200).unwrap()[&recipient],
            older.id
        );
    }

    #[test]
    fn read_arrival_head_cannot_make_a_remaining_held_reply_alert_eligible() {
        let (mut registry, _directory) = registry();
        let recipient = Lineage(TerminalId::new());
        let other = Lineage(TerminalId::new());
        let mut ask = message(party(recipient), party(other), 1);
        ask.wait_until_ms = Some(100);
        registry.post_message(&ask).unwrap();
        let mut held = message(party(other), party(recipient), 2);
        held.reply_to = Some(ask.id);
        registry.post_message(&held).unwrap();
        let ordinary = message(Party::user(), party(recipient), 3);
        registry.post_message(&ordinary).unwrap();
        assert_eq!(
            registry.reconcile_message_attention(50).unwrap()[&recipient],
            ordinary.id
        );
        registry
            .acknowledge_messages(
                Some(recipient),
                None,
                &Receipt {
                    ids: vec![ordinary.id],
                },
                50,
            )
            .unwrap();
        assert!(
            registry
                .pending_message_lineages()
                .unwrap()
                .contains(&recipient)
        );
        assert!(
            !registry
                .reconcile_message_attention(50)
                .unwrap()
                .contains_key(&recipient)
        );
        // Release preserves the durable arrival head rather than inventing
        // a new identity merely because a subset was read.
        assert_eq!(
            registry.reconcile_message_attention(100).unwrap()[&recipient],
            ordinary.id
        );
    }

    #[test]
    fn expired_mail_retires_attention_and_pruned_heads_remain_valid() {
        let (mut registry, _directory) = registry();
        let recipient = Lineage(TerminalId::new());
        let retained = message(Party::user(), party(recipient), 100);
        let expired_head = message(Party::user(), party(recipient), 1);
        registry.post_message(&retained).unwrap();
        registry.post_message(&expired_head).unwrap();
        assert_eq!(registry.prune_messages(50).unwrap(), 1);
        assert_eq!(
            registry.reconcile_message_attention(200).unwrap()[&recipient],
            retained.id
        );
        assert_eq!(registry.prune_messages(101).unwrap(), 1);
        assert!(registry.pending_message_lineages().unwrap().is_empty());
        assert!(
            registry
                .reconcile_message_attention(200)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn attention_arrivals_survive_partial_reads_restart_and_clock_reversal() {
        let (mut registry, directory) = registry();
        let recipient = Lineage(TerminalId::new());
        let first = message(Party::user(), party(recipient), 100);
        let second = message(Party::user(), party(recipient), 50);
        registry.post_message(&first).unwrap();
        registry.post_message(&second).unwrap();
        assert_eq!(
            registry.reconcile_message_attention(200).unwrap()[&recipient],
            second.id
        );
        registry
            .acknowledge_messages(
                Some(recipient),
                Some(recipient.0),
                &Receipt {
                    ids: vec![second.id],
                },
                200,
            )
            .unwrap();
        assert_eq!(
            registry.reconcile_message_attention(200).unwrap()[&recipient],
            second.id
        );
        assert_eq!(
            registry
                .peek_messages(
                    Some(recipient),
                    &Inbox {
                        all: false,
                        replies_to: None
                    }
                )
                .unwrap()
                .len(),
            1
        );
        drop(registry);
        let mut registry = Registry::at(&directory.path().join("model.sqlite3")).unwrap();
        assert_eq!(
            registry.reconcile_message_attention(300).unwrap()[&recipient],
            second.id
        );
        registry
            .acknowledge_messages(
                Some(recipient),
                None,
                &Receipt {
                    ids: vec![first.id],
                },
                300,
            )
            .unwrap();
        assert!(registry.pending_message_lineages().unwrap().is_empty());
        assert!(
            registry
                .reconcile_message_attention(300)
                .unwrap()
                .is_empty()
        );
        let third = message(Party::user(), party(recipient), 10);
        registry.post_message(&third).unwrap();
        assert_eq!(
            registry.reconcile_message_attention(300).unwrap()[&recipient],
            third.id
        );
    }

    #[test]
    fn held_reply_keeps_pending_identity_without_hiding_older_available_mail() {
        let (mut registry, _directory) = registry();
        let recipient = Lineage(TerminalId::new());
        let other = Lineage(TerminalId::new());
        let older = message(Party::user(), party(recipient), 1);
        registry.post_message(&older).unwrap();
        let mut ask = message(party(recipient), party(other), 2);
        ask.wait_until_ms = Some(100);
        registry.post_message(&ask).unwrap();
        let mut reply = message(party(other), party(recipient), 3);
        reply.reply_to = Some(ask.id);
        registry.post_message(&reply).unwrap();
        assert_eq!(
            registry.reconcile_message_attention(50).unwrap()[&recipient],
            older.id
        );
        registry
            .acknowledge_messages(
                Some(recipient),
                None,
                &Receipt {
                    ids: vec![older.id],
                },
                50,
            )
            .unwrap();
        assert!(
            registry
                .pending_message_lineages()
                .unwrap()
                .contains(&recipient)
        );
        assert!(
            !registry
                .reconcile_message_attention(50)
                .unwrap()
                .contains_key(&recipient)
        );
        assert_eq!(
            registry.reconcile_message_attention(100).unwrap()[&recipient],
            reply.id
        );
    }

    #[test]
    fn attention_migration_uses_arrival_order_and_post_rollback_keeps_head() {
        let (mut registry, _directory) = registry();
        let recipient = Lineage(TerminalId::new());
        let first = message(Party::user(), party(recipient), 100);
        let last = message(Party::user(), party(recipient), 1);
        registry.post_message(&first).unwrap();
        registry.post_message(&last).unwrap();
        registry
            .db
            .execute_batch(
                "DROP TABLE channel_work_operations;
                DROP TABLE channel_work_entries;
                DROP TABLE channel_work_homes;
                DROP TABLE mailbox_attention_heads; PRAGMA user_version=11;",
            )
            .unwrap();
        let tx = registry
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        migrate_attention(&tx).unwrap();
        tx.pragma_update(None, "user_version", 12).unwrap();
        tx.commit().unwrap();
        assert_eq!(
            registry.reconcile_message_attention(200).unwrap()[&recipient],
            last.id
        );
        registry.db.execute_batch("CREATE TRIGGER reject_head BEFORE UPDATE ON mailbox_attention_heads BEGIN SELECT RAISE(ABORT, 'injected failure'); END;").unwrap();
        let failed = message(Party::user(), party(recipient), 200);
        assert!(registry.post_message(&failed).is_err());
        assert_eq!(
            registry.reconcile_message_attention(200).unwrap()[&recipient],
            last.id
        );
        assert_eq!(
            registry
                .peek_messages(
                    Some(recipient),
                    &Inbox {
                        all: false,
                        replies_to: None
                    }
                )
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn wire_bounded_user_inbox_batches_drain_every_body_in_oldest_order() {
        let (mut registry, _directory) = registry();
        let query = Inbox {
            all: false,
            replies_to: None,
        };
        let mut expected = Vec::new();
        for sender in 0..3 {
            let from = party(Lineage(TerminalId::new()));
            for index in 0..MAX_PENDING {
                let mut entry = message(
                    from.clone(),
                    Party::user(),
                    (sender * MAX_PENDING + index) as u64,
                );
                entry.body = format!("message {}: ", entry.id);
                entry.body.extend(std::iter::repeat_n(
                    'x',
                    crate::agent_execution::mailbox::MAX_MESSAGE_BYTES - entry.body.len(),
                ));
                registry.post_message(&entry).unwrap();
                expected.push(entry.id);
            }
        }
        let history = registry
            .peek_messages(
                None,
                &Inbox {
                    all: true,
                    replies_to: None,
                },
            )
            .unwrap();
        assert_eq!(
            history.iter().map(|entry| entry.id).collect::<Vec<_>>(),
            expected[expected.len() - RECENT..]
        );
        assert_inbox_fits_wire(&history);
        let mut received = Vec::new();
        loop {
            let batch = registry.peek_messages(None, &query).unwrap();
            if batch.is_empty() {
                break;
            }
            assert!(batch.len() <= MAX_PENDING);
            assert_inbox_fits_wire(&batch);
            for entry in &batch {
                assert_eq!(entry.state, State::Pending);
                assert_eq!(
                    entry.body.len(),
                    crate::agent_execution::mailbox::MAX_MESSAGE_BYTES
                );
                let stored = registry.message(entry.id).unwrap().unwrap();
                assert_eq!(stored.state, State::Pending);
                assert_eq!(entry.body, stored.body);
                assert!(entry.body.starts_with(&format!("message {}: ", entry.id)));
            }
            if received.is_empty() {
                let mut later = message(party(Lineage(TerminalId::new())), Party::user(), 1000);
                later.body = format!("message {}: ", later.id);
                later.body.extend(std::iter::repeat_n(
                    'x',
                    crate::agent_execution::mailbox::MAX_MESSAGE_BYTES - later.body.len(),
                ));
                registry.post_message(&later).unwrap();
                expected.push(later.id);
            }
            let ids: Vec<_> = batch.iter().map(|entry| entry.id).collect();
            registry
                .acknowledge_messages(None, None, &Receipt { ids: ids.clone() }, 2000)
                .unwrap();
            received.extend(ids);
        }
        assert_eq!(received, expected);
        for id in received {
            assert_eq!(registry.message(id).unwrap().unwrap().state, State::Read);
        }
    }

    #[test]
    fn wire_bounded_inbox_counts_metadata_before_settling_legacy_reads() {
        let (mut registry, _directory) = registry();
        let mut from = party(Lineage(TerminalId::new()));
        from.title = Some("t".repeat(128 * 1024));
        let mut expected = Vec::new();
        for index in 0..MAX_PENDING {
            let mut entry = message(from.clone(), Party::user(), index as u64);
            entry.body = "x".repeat(crate::agent_execution::mailbox::MAX_MESSAGE_BYTES);
            registry.post_message(&entry).unwrap();
            expected.push(entry.id);
        }
        let query = Inbox {
            all: false,
            replies_to: None,
        };
        let first = registry.read_messages(None, None, &query, 1000).unwrap();
        assert!(
            !first.is_empty() && first.len() < MAX_PENDING,
            "a row cap alone cannot bound a reply with large metadata"
        );
        assert_inbox_fits_wire(&first);
        for entry in &first {
            assert_eq!(entry.state, State::Read);
            assert_eq!(entry.from.title.as_ref().unwrap().len(), 128 * 1024);
        }
        for id in &expected[first.len()..] {
            assert_eq!(
                registry.message(*id).unwrap().unwrap().state,
                State::Pending
            );
        }
        let second = registry.read_messages(None, None, &query, 1001).unwrap();
        assert_inbox_fits_wire(&second);
        assert_eq!(
            first
                .iter()
                .chain(&second)
                .map(|entry| entry.id)
                .collect::<Vec<_>>(),
            expected
        );
        assert!(registry.peek_messages(None, &query).unwrap().is_empty());
    }

    #[test]
    fn wire_bounded_inbox_preserves_an_individually_oversized_record() {
        let (mut registry, _directory) = registry();
        let mut entry = message(Party::user(), Party::user(), 0);
        entry.from.title = Some("t".repeat(crate::terminal::ipc::MAX_FRAME_BYTES));
        registry.post_message(&entry).unwrap();
        let query = Inbox {
            all: false,
            replies_to: None,
        };
        assert!(
            registry
                .peek_messages(None, &query)
                .unwrap_err()
                .contains("state is unchanged")
        );
        assert!(registry.read_messages(None, None, &query, 1).is_err());
        assert_eq!(
            registry.message(entry.id).unwrap().unwrap().state,
            State::Pending
        );
    }

    #[test]
    fn the_key_is_made_once_and_survives_reopening() {
        let (registry, directory) = registry();
        let key = registry.mailbox_key().unwrap();
        assert_ne!(key, [0; 32]);
        drop(registry);
        let reopened = Registry::at(&directory.path().join("model.sqlite3")).unwrap();
        assert_eq!(reopened.mailbox_key().unwrap(), key);
        let (other, _directory) = self::registry();
        assert_ne!(other.mailbox_key().unwrap(), key);
    }

    #[test]
    fn a_message_settles_once_whether_delivered_or_read() {
        let (mut registry, _directory) = registry();
        let (a, b, c) = (
            Lineage(TerminalId::new()),
            Lineage(TerminalId::new()),
            Lineage(TerminalId::new()),
        );
        let first = message(party(a), party(b), 1);
        let second = message(Party::user(), party(b), 2);
        let other = message(party(a), party(c), 3);
        registry.post_message(&first).unwrap();
        registry.post_message(&second).unwrap();
        registry.post_message(&other).unwrap();
        let ids = |registry: &Registry, at| {
            registry
                .deliverable_messages(at)
                .unwrap()
                .0
                .into_iter()
                .map(|m| m.id)
                .collect::<Vec<_>>()
        };
        // Each recipient's oldest first; the next waits for it to settle.
        assert_eq!(ids(&registry, 10), [first.id, other.id]);
        assert_eq!(registry.deliverable_messages(10).unwrap().1, None);

        let session = TerminalId::new();
        assert!(registry.deliver_message(first.id, session, 11).unwrap());
        assert!(!registry.deliver_message(first.id, session, 12).unwrap());
        assert_eq!(ids(&registry, 12), [second.id, other.id]);
        // Reading takes what was not delivered, and only once.
        let unread = registry
            .read_messages(
                Some(b),
                Some(session),
                &Inbox {
                    all: false,
                    replies_to: None,
                },
                13,
            )
            .unwrap();
        assert_eq!(unread.iter().map(|m| m.id).collect::<Vec<_>>(), [second.id]);
        assert_eq!(unread[0].state, State::Read);
        assert!(!registry.deliver_message(second.id, session, 14).unwrap());
        assert_eq!(ids(&registry, 15), [other.id]);
        let all = registry
            .read_messages(
                Some(b),
                None,
                &Inbox {
                    all: true,
                    replies_to: None,
                },
                16,
            )
            .unwrap();
        assert_eq!(
            all.iter().map(|m| (m.id, m.state)).collect::<Vec<_>>(),
            [(first.id, State::Delivered), (second.id, State::Read)]
        );
        assert_eq!(
            registry.message(first.id).unwrap().unwrap().to.session,
            Some(session)
        );
    }

    #[test]
    fn disconnected_peeks_leave_messages_pending_until_an_exact_receipt() {
        let (mut registry, _directory) = registry();
        let recipient = Lineage(TerminalId::new());
        let mut first = message(Party::user(), party(recipient), 1);
        first.to.session = None;
        registry.post_message(&first).unwrap();
        let query = Inbox {
            all: false,
            replies_to: None,
        };
        let disconnected = registry.peek_messages(Some(recipient), &query).unwrap();
        assert_eq!(disconnected, vec![first.clone()]);
        assert_eq!(
            registry.peek_messages(Some(recipient), &query).unwrap(),
            disconnected
        );
        let mut second = message(Party::user(), party(recipient), 2);
        second.to.session = None;
        registry.post_message(&second).unwrap();
        let session = TerminalId::new();
        let receipt = Receipt {
            ids: vec![first.id],
        };
        registry
            .acknowledge_messages(Some(recipient), Some(session), &receipt, 3)
            .unwrap();
        registry
            .acknowledge_messages(Some(recipient), None, &receipt, 4)
            .unwrap();
        let settled = registry.message(first.id).unwrap().unwrap();
        assert_eq!(settled.state, State::Read);
        assert_eq!(settled.to.session, Some(session));
        assert_eq!(
            registry.peek_messages(Some(recipient), &query).unwrap(),
            vec![second]
        );
    }

    #[test]
    fn receipts_are_bounded_and_recipient_scoped_without_partial_settlement() {
        let (mut registry, _directory) = registry();
        let recipient = Lineage(TerminalId::new());
        let mine = message(Party::user(), party(recipient), 1);
        let foreign = message(party(recipient), Party::user(), 2);
        registry.post_message(&mine).unwrap();
        registry.post_message(&foreign).unwrap();
        for ids in [vec![mine.id, foreign.id], vec![mine.id; MAX_PENDING + 1]] {
            assert!(
                registry
                    .acknowledge_messages(Some(recipient), None, &Receipt { ids }, 3)
                    .is_err()
            );
            assert_eq!(
                registry.message(mine.id).unwrap().unwrap().state,
                State::Pending
            );
            assert_eq!(
                registry.message(foreign.id).unwrap().unwrap().state,
                State::Pending
            );
        }
        assert!(
            registry
                .acknowledge_messages(None, None, &Receipt { ids: vec![mine.id] }, 4)
                .is_err()
        );
        registry
            .acknowledge_messages(
                None,
                None,
                &Receipt {
                    ids: vec![foreign.id],
                },
                5,
            )
            .unwrap();
        assert_eq!(
            registry.message(foreign.id).unwrap().unwrap().state,
            State::Read
        );
    }

    #[test]
    fn a_reply_waits_for_its_asker_until_the_asker_stops_waiting() {
        let (mut registry, _directory) = registry();
        let (a, b) = (Lineage(TerminalId::new()), Lineage(TerminalId::new()));
        let mut question = message(party(a), party(b), 1);
        question.wait_until_ms = Some(100);
        registry.post_message(&question).unwrap();
        let mut reply = message(party(b), party(a), 2);
        reply.reply_to = Some(question.id);
        registry.post_message(&reply).unwrap();

        let (deliverable, released) = registry.deliverable_messages(50).unwrap();
        assert_eq!(
            deliverable.iter().map(|m| m.id).collect::<Vec<_>>(),
            [question.id]
        );
        assert_eq!(released, Some(100));
        // An unrelated read leaves the reply for its asker...
        let replies = Inbox {
            all: false,
            replies_to: Some(question.id),
        };
        assert!(
            registry
                .read_messages(Some(b), None, &replies, 51)
                .unwrap()
                .is_empty()
        );
        // ...which takes it.
        let read = registry.read_messages(Some(a), None, &replies, 52).unwrap();
        assert_eq!(read.iter().map(|m| m.id).collect::<Vec<_>>(), [reply.id]);

        // An asker that gave up has its reply delivered after all.
        let mut late = message(party(b), party(a), 3);
        late.reply_to = Some(question.id);
        registry.post_message(&late).unwrap();
        let ids = |at| {
            registry
                .deliverable_messages(at)
                .unwrap()
                .0
                .into_iter()
                .map(|m| m.id)
                .collect::<Vec<_>>()
        };
        assert!(!ids(99).contains(&late.id));
        assert!(ids(100).contains(&late.id));
    }

    #[test]
    fn courier_selection_preserves_timestamp_ties_and_held_reply_payloads() {
        let (mut registry, _directory) = registry();
        let mut lineages = [
            Lineage(TerminalId::new()),
            Lineage(TerminalId::new()),
            Lineage(TerminalId::new()),
        ];
        lineages.sort_by_key(|lineage| lineage.to_string());
        let [a, b, c] = lineages;
        let mut question = message(party(a), Party::user(), 1);
        question.wait_until_ms = Some(500);
        registry.post_message(&question).unwrap();

        // Insert the higher lineage first, so rowid order differs from the
        // partition order for messages with identical creation timestamps.
        let mut b_first = message(party(c), party(b), 100);
        b_first.from.title = Some("Sender with metadata".into());
        b_first.body = "The complete body survives selection: é\nsecond line".into();
        registry.post_message(&b_first).unwrap();
        let mut held = message(party(b), party(a), 100);
        held.reply_to = Some(question.id);
        held.body = "A held reply".into();
        registry.post_message(&held).unwrap();
        let next = message(party(c), party(a), 100);
        registry.post_message(&next).unwrap();
        let earlier = message(Party::user(), party(c), 99);
        registry.post_message(&earlier).unwrap();

        // Pending recipients have not yet received an authenticated session.
        for item in [&mut b_first, &mut held] {
            item.to.session = None;
        }
        let pending_copy = |mut item: Message| {
            item.to.session = None;
            item
        };
        assert_eq!(
            registry.deliverable_messages(499).unwrap(),
            (
                vec![
                    pending_copy(earlier.clone()),
                    pending_copy(next),
                    b_first.clone()
                ],
                Some(500)
            )
        );
        // At release, the older rowid within a lineage wins the timestamp tie.
        assert_eq!(
            registry.deliverable_messages(500).unwrap(),
            (vec![pending_copy(earlier), held, b_first], None)
        );
    }

    #[test]
    fn replies_resolve_by_prefix_among_the_callers_own_messages() {
        let (mut registry, _directory) = registry();
        let (a, b) = (Lineage(TerminalId::new()), Lineage(TerminalId::new()));
        let mine = message(party(a), party(b), 1);
        registry.post_message(&mine).unwrap();
        let prefix = &mine.id.to_string()[..8];
        assert_eq!(
            registry.received_message(Some(b), prefix).unwrap().id,
            mine.id
        );
        assert_eq!(
            registry
                .received_message(Some(b), &prefix.to_ascii_uppercase())
                .unwrap()
                .id,
            mine.id
        );
        // Another lineage, the user, and a pattern are refused.
        assert!(registry.received_message(Some(a), prefix).is_err());
        assert!(registry.received_message(None, prefix).is_err());
        assert!(registry.received_message(Some(b), "%").is_err());
    }

    #[test]
    fn a_recipient_holds_a_bounded_queue_and_old_messages_are_pruned() {
        let (mut registry, _directory) = registry();
        let b = Lineage(TerminalId::new());
        for at in 0..MAX_PENDING as u64 {
            registry
                .post_message(&message(Party::user(), party(b), at))
                .unwrap();
        }
        let refused = registry
            .post_message(&message(Party::user(), party(b), 100))
            .unwrap_err();
        assert!(refused.contains("messages waiting"), "{refused}");
        // The user's queue is separate, and each sender has its own share.
        let a = Lineage(TerminalId::new());
        for at in 0..MAX_PENDING as u64 {
            registry
                .post_message(&message(party(b), Party::user(), at))
                .unwrap();
        }
        let refused = registry
            .post_message(&message(party(b), Party::user(), 100))
            .unwrap_err();
        assert!(refused.contains("has not read your last"), "{refused}");
        registry
            .post_message(&message(party(a), Party::user(), 100))
            .unwrap();
        assert_eq!(registry.prune_messages(10).unwrap(), 20);
        registry
            .post_message(&message(Party::user(), party(b), 101))
            .unwrap();
    }
}
