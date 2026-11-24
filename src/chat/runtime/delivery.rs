//! The daemon keeps recent delivery keys across actor replacement and restart.
//! Record each key before admitting work, then save the acknowledgement before
//! returning it to the sender. A crash can leave a reservation without an
//! acknowledgement. That key can never prove that its work was refused.
//! If the file is unavailable, the daemon can still remember keys in memory
//! until it can open the file again.

use super::Rejection;
use crate::{
    chat::{
        DeliveryKey,
        failure::delivery::{
            DELIVERY_HISTORY_UNAVAILABLE, DELIVERY_NOT_SENT, LOST_DELIVERY_OUTCOME,
        },
    },
    instance::OwnedLock,
    model::TerminalId,
};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use std::{
    collections::{HashMap, hash_map::Entry},
    path::{Path, PathBuf},
    sync::Mutex,
    time::Duration,
};

type Outcome = Result<(), Rejection>;
pub(crate) type Reply = crossbeam_channel::Receiver<Outcome>;

const KEPT_PER_CHAT: i64 = 64;
const KEPT_TOTAL: i64 = 16_384;
const MAX_ERROR_BYTES: usize = 1024;
const SCHEMA_VERSION: u32 = 1;
const CROWDED: &str = "Too many messages are awaiting confirmation, so this was not sent.";

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct Key(TerminalId, DeliveryKey);

impl Key {
    fn sql(self) -> (String, String) {
        (self.0.to_string(), self.1.to_string())
    }
}

enum Pending {
    Sending,
    Waiting(Reply, fn() -> Rejection),
    Saving,
    Settled(Outcome),
}

struct Ledger {
    db: Connection,
    _lock: Option<OwnedLock>,
}

pub(crate) struct Deliveries {
    ledger: Mutex<Ledger>,
    pending: Mutex<HashMap<Key, Pending>>,
}

impl Deliveries {
    pub(crate) fn open() -> Result<Self, String> {
        Self::at(&ledger_path()?)
    }

    fn at(path: &Path) -> Result<Self, String> {
        Ledger::at(path).map(Self::new)
    }

    pub(crate) fn in_memory() -> Result<Self, String> {
        let db = Connection::open_in_memory().map_err(|error| error.to_string())?;
        initialize(&db).map_err(|error| error.to_string())?;
        Ok(Self::new(Ledger { db, _lock: None }))
    }

    fn new(ledger: Ledger) -> Self {
        Self {
            ledger: Mutex::new(ledger),
            pending: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) fn restore_persistence(&self) -> Result<(), String> {
        self.restore_at(&ledger_path()?)
    }

    fn restore_at(&self, path: &Path) -> Result<(), String> {
        // Opening a file that is still unavailable must leave the fallback
        // free to serve sends. Only the final copy and switch need its guard.
        let mut disk = Ledger::at(path)?;
        let mut current = self.ledger.lock().unwrap_or_else(|p| p.into_inner());
        disk.import(&current).map_err(|error| error.to_string())?;
        *current = disk;
        Ok(())
    }

    /// Reuse a pending or settled delivery for this chat. Only a refusal
    /// proves the original attempt could safely be admitted again.
    pub(crate) fn deliver(
        &self,
        chat: TerminalId,
        key: DeliveryKey,
        wait: Duration,
        unknown: fn() -> Rejection,
        admit: impl FnOnce() -> Result<Reply, Rejection>,
    ) -> Outcome {
        let key = Key(chat, key);
        self.collect_replies();
        let earlier = {
            let mut pending = self.pending.lock().unwrap_or_else(|p| p.into_inner());
            match pending.entry(key) {
                Entry::Occupied(mut entry) => match entry.get() {
                    Pending::Sending => return Err(unknown()),
                    Pending::Saving | Pending::Settled(_) => return Err(history_unavailable()),
                    Pending::Waiting(reply, _) => {
                        let reply = reply.clone();
                        entry.insert(Pending::Sending);
                        Some(reply)
                    }
                },
                Entry::Vacant(entry) => {
                    entry.insert(Pending::Sending);
                    None
                }
            }
        };

        let await_reply = |reply: Reply| match reply.recv_timeout(wait) {
            Ok(result) => Ok(result),
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => Err(reply),
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => Ok(Err(unknown())),
        };
        let earlier = earlier.map(await_reply);
        let retry = earlier.is_some();
        let outcome = match earlier {
            Some(Ok(Err(Rejection::Refused(_)))) | None => {
                // A refusal lets us reuse an unsettled reservation. A disk
                // ledger restored since then might already know this key.
                if let Some(result) = self.reserve(key, retry) {
                    return result;
                }
                match admit() {
                    Ok(reply) => await_reply(reply),
                    Err(refusal) => Ok(Err(refusal)),
                }
            }
            Some(outcome) => outcome,
        };

        match outcome {
            Err(reply) => {
                self.pending
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .insert(key, Pending::Waiting(reply, unknown));
                Err(unknown())
            }
            Ok(result) => self.finish(key, result),
        }
    }

    fn reserve(&self, key: Key, retry: bool) -> Option<Outcome> {
        let reservation = self
            .ledger
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .reserve(key, retry);
        let (result, retry_settlement) = match reservation {
            Ok(Reservation::New) => return None,
            Ok(Reservation::Known(result)) => (result, false),
            Ok(Reservation::Full) => (Err(Rejection::Refused(CROWDED.into())), false),
            Err(error) => {
                tracing::error!(chat = %key.0, %error, "cannot reserve a message delivery key");
                (Err(Rejection::Refused(DELIVERY_NOT_SENT.into())), retry)
            }
        };
        let mut pending = self.pending.lock().unwrap_or_else(|p| p.into_inner());
        if retry_settlement {
            // The old attempt was refused. Release its existing reservation
            // once the ledger can be written again.
            pending.insert(key, Pending::Settled(result.clone()));
        } else {
            pending.remove(&key);
        }
        Some(result)
    }

    /// Keep each ready key claimed while saving it outside the pending lock.
    fn collect_replies(&self) {
        let ready: Vec<_> = self
            .pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter_mut()
            .filter_map(|(&key, pending)| {
                let result = match pending {
                    Pending::Waiting(reply, unknown) => match reply.try_recv() {
                        Ok(result) => result,
                        Err(crossbeam_channel::TryRecvError::Disconnected) => Err(unknown()),
                        Err(crossbeam_channel::TryRecvError::Empty) => return None,
                    },
                    Pending::Settled(_) => {
                        let Pending::Settled(result) = std::mem::replace(pending, Pending::Saving)
                        else {
                            unreachable!()
                        };
                        result
                    }
                    Pending::Sending | Pending::Saving => return None,
                };
                *pending = Pending::Saving;
                Some((key, result))
            })
            .collect();
        for (key, result) in ready {
            let _ = self.finish(key, result);
        }
    }

    fn finish(&self, key: Key, result: Outcome) -> Outcome {
        let saved = self
            .ledger
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .save_outcome(key, &result);
        let mut pending = self.pending.lock().unwrap_or_else(|p| p.into_inner());
        match saved {
            Ok(prior) => {
                pending.remove(&key);
                prior.unwrap_or(result)
            }
            Err(error) => {
                // Retry on the next request. The reservation prevents replay.
                pending.insert(key, Pending::Settled(result));
                drop(pending);
                tracing::error!(chat = %key.0, %error, "cannot save a message acknowledgement");
                Err(history_unavailable())
            }
        }
    }
}

fn ledger_path() -> Result<PathBuf, String> {
    let dirs = crate::instance::project_dirs().ok_or("the data directory is unavailable")?;
    Ok(dirs.data_dir().join("chat-deliveries.sqlite3"))
}

fn initialize(db: &Connection) -> rusqlite::Result<()> {
    let tx = db.unchecked_transaction()?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS chat_deliveries (
             ordinal INTEGER PRIMARY KEY,
             chat TEXT NOT NULL,
             key TEXT NOT NULL,
             settled INTEGER NOT NULL,
             error TEXT,
             UNIQUE(chat, key)
         );
         CREATE INDEX IF NOT EXISTS chat_deliveries_by_chat ON chat_deliveries(chat, ordinal);",
    )?;
    tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    tx.commit()
}

fn history_unavailable() -> Rejection {
    Rejection::Unacknowledged(DELIVERY_HISTORY_UNAVAILABLE.into())
}

fn stored_outcome(error: Option<String>) -> Outcome {
    error.map_or(Ok(()), |error| Err(Rejection::Unacknowledged(error)))
}

enum Reservation {
    New,
    Known(Outcome),
    Full,
}

impl Ledger {
    fn at(path: &Path) -> Result<Self, String> {
        crate::secure_file::prepare_private_directory(path.parent().expect("ledger directory"))
            .map_err(|error| error.to_string())?;
        // Daemons with distinct runtime directories can share this data
        // directory. Only the lock owner may touch the database or its WAL.
        let lock = crate::instance::owned_lock(
            &path.with_extension("lock"),
            libc::LOCK_EX | libc::LOCK_NB,
        )
        .map_err(|error| error.to_string())?;
        for suffix in ["-wal", "-shm", "-journal"] {
            let mut sidecar = path.as_os_str().to_os_string();
            sidecar.push(suffix);
            crate::secure_file::migrate_existing_private_file(Path::new(&sidecar))
                .map_err(|error| error.to_string())?;
        }
        drop(
            crate::secure_file::open_private_append_file(path)
                .map_err(|error| error.to_string())?,
        );
        let db = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )
        .map_err(|error| error.to_string())?;
        db.busy_timeout(Duration::ZERO)
            .map_err(|error| error.to_string())?;
        let version: u32 = db
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(|error| error.to_string())?;
        if version > SCHEMA_VERSION {
            return Err(format!(
                "unsupported message delivery ledger schema {version}"
            ));
        }
        let mode: String = db
            .pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))
            .map_err(|error| error.to_string())?;
        if !mode.eq_ignore_ascii_case("wal") {
            return Err(format!(
                "message delivery ledger requires WAL, found {mode}"
            ));
        }
        // WAL with NORMAL survives a daemon crash without forcing a disk
        // sync for every send. Clients keep delivery keys only in memory.
        db.pragma_update(None, "synchronous", "NORMAL")
            .map_err(|error| error.to_string())?;
        if version < SCHEMA_VERSION {
            initialize(&db).map_err(|error| error.to_string())?;
        }
        // Reservations left by the previous daemon did not receive a
        // durable final acknowledgement.
        db.execute(
            "UPDATE chat_deliveries SET settled=1, error=?1 WHERE settled=0",
            [LOST_DELIVERY_OUTCOME],
        )
        .map_err(|error| error.to_string())?;
        Ok(Self {
            db,
            _lock: Some(lock),
        })
    }

    fn import(&mut self, fallback: &Self) -> rusqlite::Result<()> {
        let mut read = fallback
            .db
            .prepare("SELECT chat, key, settled, error FROM chat_deliveries ORDER BY ordinal")?;
        let tx = self.db.transaction()?;
        {
            let mut write = tx.prepare(
                "INSERT INTO chat_deliveries(chat, key, settled, error) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(chat, key) DO UPDATE SET ordinal=excluded.ordinal, error=CASE
                     WHEN excluded.settled=1 AND excluded.error IS NULL THEN NULL
                     ELSE chat_deliveries.error END",
            )?;
            // A key used during the outage may already have a disk outcome.
            // A later uncertain send cannot erase that history.
            let mut rows = read.query([])?;
            while let Some(row) = rows.next()? {
                write.execute(params![
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, bool>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ])?;
            }
        }
        // Apply the normal bounds to the combined history, keeping every
        // outstanding reservation ahead of settled rows when pruning.
        tx.execute(
            "DELETE FROM chat_deliveries WHERE ordinal IN (
                SELECT ordinal FROM (
                    SELECT ordinal, row_number() OVER (
                        PARTITION BY chat ORDER BY settled, ordinal DESC
                    ) AS position FROM chat_deliveries
                ) WHERE position > ?1
             )",
            [KEPT_PER_CHAT],
        )?;
        tx.execute(
            "DELETE FROM chat_deliveries WHERE ordinal IN (
                SELECT ordinal FROM chat_deliveries
                ORDER BY settled, ordinal DESC LIMIT -1 OFFSET ?1
             )",
            [KEPT_TOTAL],
        )?;
        tx.commit()
    }

    fn reserve(&mut self, key: Key, retry: bool) -> rusqlite::Result<Reservation> {
        let (chat, delivery) = key.sql();
        if let Some((settled, error)) = self
            .db
            .query_row(
                "SELECT settled, error FROM chat_deliveries WHERE chat=?1 AND key=?2",
                params![chat, delivery],
                |row| Ok((row.get::<_, bool>(0)?, row.get::<_, Option<String>>(1)?)),
            )
            .optional()?
        {
            let result = match (settled, error) {
                (true, error) => stored_outcome(error),
                (false, _) if retry => return Ok(Reservation::New),
                // A reservation outlives a failed attempt to write its outcome.
                (false, _) => Err(Rejection::Unacknowledged(LOST_DELIVERY_OUTCOME.into())),
            };
            return Ok(Reservation::Known(result));
        }

        let transaction = self.db.transaction()?;
        let in_chat: i64 = transaction.query_row(
            "SELECT count(*) FROM chat_deliveries WHERE chat=?1",
            [&chat],
            |row| row.get(0),
        )?;
        if in_chat >= KEPT_PER_CHAT
            && transaction.execute(
                "DELETE FROM chat_deliveries WHERE ordinal=(
                    SELECT ordinal FROM chat_deliveries WHERE chat=?1 AND settled=1
                    ORDER BY ordinal LIMIT 1
                 )",
                [&chat],
            )? == 0
        {
            return Ok(Reservation::Full);
        }
        let total: i64 =
            transaction.query_row("SELECT count(*) FROM chat_deliveries", [], |row| row.get(0))?;
        if total >= KEPT_TOTAL
            && transaction.execute(
                "DELETE FROM chat_deliveries WHERE ordinal=(
                    SELECT ordinal FROM chat_deliveries WHERE settled=1 ORDER BY ordinal LIMIT 1
                 )",
                [],
            )? == 0
        {
            return Ok(Reservation::Full);
        }
        transaction.execute(
            "INSERT INTO chat_deliveries(chat, key, settled) VALUES (?1, ?2, 0)",
            params![chat, delivery],
        )?;
        transaction.commit()?;
        Ok(Reservation::New)
    }

    fn save_outcome(&self, key: Key, result: &Outcome) -> rusqlite::Result<Option<Outcome>> {
        let (chat, delivery) = key.sql();
        let error = match result {
            Ok(()) => None,
            Err(Rejection::Unacknowledged(error)) => {
                Some(&error[..error.floor_char_boundary(MAX_ERROR_BYTES)])
            }
            Err(Rejection::Refused(_)) => None,
        };
        let updated = if matches!(result, Err(Rejection::Refused(_))) {
            self.db.execute(
                "DELETE FROM chat_deliveries WHERE chat=?1 AND key=?2 AND settled=0",
                params![chat, delivery],
            )?
        } else {
            self.db.execute(
                "UPDATE chat_deliveries SET settled=1, error=?3 WHERE chat=?1 AND key=?2
                   AND (settled=0 OR (error IS NOT NULL AND ?3 IS NULL))",
                params![chat, delivery, error],
            )?
        };
        // A settled row from disk outranks a fallback refusal or uncertainty.
        if updated > 0 {
            return Ok(None);
        }
        self.db
            .query_row(
                "SELECT error FROM chat_deliveries WHERE chat=?1 AND key=?2 AND settled=1",
                params![chat, delivery],
                |row| row.get(0),
            )
            .optional()
            .map(|error| error.map(stored_outcome))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    const WAIT: Duration = Duration::from_millis(20);

    fn unknown() -> Rejection {
        Rejection::Unacknowledged("not confirmed".into())
    }

    /// A send that is admitted, and the sender of its acknowledgement.
    fn admitted() -> (
        crossbeam_channel::Sender<Result<(), Rejection>>,
        impl FnOnce() -> Result<Reply, Rejection>,
    ) {
        let (sender, reply) = crossbeam_channel::bounded(1);
        (sender, move || Ok(reply))
    }

    #[test]
    fn a_send_asked_for_again_waits_on_the_first_instead_of_sending_twice() {
        let deliveries = Deliveries::in_memory().unwrap();
        let chat = TerminalId::new();
        let key = DeliveryKey::new_v4();
        let (acknowledge, admit) = admitted();
        assert_eq!(
            deliveries.deliver(chat, key, WAIT, unknown, admit),
            Err(unknown())
        );
        let sent = Cell::new(0);
        let again = || {
            sent.set(sent.get() + 1);
            Err(Rejection::Refused("sent twice".into()))
        };
        // Still unacknowledged: asking again reports it unknown again.
        assert_eq!(
            deliveries.deliver(chat, key, WAIT, unknown, again),
            Err(unknown())
        );
        acknowledge.send(Ok(())).unwrap();
        let again = || {
            sent.set(sent.get() + 1);
            Err(Rejection::Refused("sent twice".into()))
        };
        assert_eq!(deliveries.deliver(chat, key, WAIT, unknown, again), Ok(()));
        let again = || {
            sent.set(sent.get() + 1);
            Err(Rejection::Refused("sent twice".into()))
        };
        assert_eq!(deliveries.deliver(chat, key, WAIT, unknown, again), Ok(()));
        assert_eq!(sent.get(), 0);
    }

    #[test]
    fn a_refused_send_is_sent_again_when_asked_for_again() {
        let deliveries = Deliveries::in_memory().unwrap();
        let chat = TerminalId::new();
        let key = DeliveryKey::new_v4();
        let (acknowledge, admit) = admitted();
        assert_eq!(
            deliveries.deliver(chat, key, WAIT, unknown, admit),
            Err(unknown())
        );
        acknowledge
            .send(Err(Rejection::Refused("not sent".into())))
            .unwrap();
        let (acknowledge, admit) = admitted();
        acknowledge.send(Ok(())).unwrap();
        assert_eq!(deliveries.deliver(chat, key, WAIT, unknown, admit), Ok(()));

        let key = DeliveryKey::new_v4();
        let refuse = || Err(Rejection::Refused("busy".into()));
        assert_eq!(
            deliveries.deliver(chat, key, WAIT, unknown, refuse),
            Err(Rejection::Refused("busy".into()))
        );
        let (acknowledge, admit) = admitted();
        acknowledge.send(Ok(())).unwrap();
        assert_eq!(deliveries.deliver(chat, key, WAIT, unknown, admit), Ok(()));
    }

    #[test]
    fn an_outcome_that_can_no_longer_be_learned_is_never_sent_again() {
        let deliveries = Deliveries::in_memory().unwrap();
        let chat = TerminalId::new();
        let key = DeliveryKey::new_v4();
        let (acknowledge, admit) = admitted();
        drop(acknowledge);
        assert_eq!(
            deliveries.deliver(chat, key, WAIT, unknown, admit),
            Err(unknown())
        );
        let resend = || -> Result<Reply, Rejection> { panic!("sent twice") };
        assert_eq!(
            deliveries.deliver(chat, key, WAIT, unknown, resend),
            Err(unknown())
        );

        let key = DeliveryKey::new_v4();
        let (acknowledge, admit) = admitted();
        let stopped = Rejection::Unacknowledged("stopped".into());
        acknowledge.send(Err(stopped.clone())).unwrap();
        assert_eq!(
            deliveries.deliver(chat, key, WAIT, unknown, admit),
            Err(stopped.clone())
        );
        let resend = || -> Result<Reply, Rejection> { panic!("sent twice") };
        assert_eq!(
            deliveries.deliver(chat, key, WAIT, unknown, resend),
            Err(stopped)
        );
    }

    #[test]
    fn only_recent_keys_are_remembered() {
        let deliveries = Deliveries::in_memory().unwrap();
        let chat = TerminalId::new();
        let first = DeliveryKey::new_v4();
        let (acknowledge, admit) = admitted();
        acknowledge.send(Ok(())).unwrap();
        assert_eq!(
            deliveries.deliver(chat, first, WAIT, unknown, admit),
            Ok(())
        );
        for _ in 0..KEPT_PER_CHAT {
            let (acknowledge, admit) = admitted();
            acknowledge.send(Ok(())).unwrap();
            assert_eq!(
                deliveries.deliver(chat, DeliveryKey::new_v4(), WAIT, unknown, admit),
                Ok(())
            );
        }
        assert_eq!(
            deliveries
                .ledger
                .lock()
                .unwrap()
                .db
                .query_row("SELECT count(*) FROM chat_deliveries", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            KEPT_PER_CHAT,
        );
        let (acknowledge, admit) = admitted();
        acknowledge.send(Ok(())).unwrap();
        assert_eq!(
            deliveries.deliver(chat, first, WAIT, unknown, admit),
            Ok(())
        );
    }

    #[test]
    fn a_full_ledger_preserves_unsettled_sends_and_collects_late_replies() {
        let deliveries = Deliveries::in_memory().unwrap();
        let chat = TerminalId::new();
        let mut acknowledgements = Vec::new();
        for _ in 0..KEPT_PER_CHAT {
            let (acknowledge, admit) = admitted();
            assert_eq!(
                deliveries.deliver(chat, DeliveryKey::new_v4(), Duration::ZERO, unknown, admit),
                Err(unknown()),
            );
            acknowledgements.push(acknowledge);
        }
        let not_admitted = || panic!("a new send cannot displace an unsettled one");
        let result = deliveries.deliver(chat, DeliveryKey::new_v4(), WAIT, unknown, not_admitted);
        assert!(matches!(result, Err(Rejection::Refused(_))));
        acknowledgements[0].send(Ok(())).unwrap();
        let (acknowledge, admit) = admitted();
        acknowledge.send(Ok(())).unwrap();
        assert_eq!(
            deliveries.deliver(chat, DeliveryKey::new_v4(), WAIT, unknown, admit),
            Ok(()),
        );
    }

    #[test]
    fn a_write_failure_keeps_the_acknowledgement_for_the_next_request() {
        let deliveries = Deliveries::in_memory().unwrap();
        let chat = TerminalId::new();
        let key = DeliveryKey::new_v4();
        deliveries
            .ledger
            .lock()
            .unwrap()
            .db
            .execute_batch(
                "CREATE TRIGGER fail_settlement BEFORE UPDATE ON chat_deliveries
             BEGIN SELECT RAISE(FAIL, 'fixture write failure'); END;",
            )
            .unwrap();

        let (acknowledge, admit) = admitted();
        acknowledge.send(Ok(())).unwrap();
        assert_eq!(
            deliveries.deliver(chat, key, WAIT, unknown, admit),
            Err(history_unavailable()),
        );
        let do_not_send = || panic!("the stored reservation prevents replay");
        assert_eq!(
            deliveries.deliver(chat, key, WAIT, unknown, do_not_send),
            Err(history_unavailable()),
        );
        deliveries
            .ledger
            .lock()
            .unwrap()
            .db
            .execute_batch("DROP TRIGGER fail_settlement")
            .unwrap();
        assert_eq!(
            deliveries.deliver(chat, key, WAIT, unknown, do_not_send),
            Ok(())
        );
    }

    #[test]
    fn a_failed_reservation_refuses_work_and_leaves_the_key_retryable() {
        let deliveries = Deliveries::in_memory().unwrap();
        let chat = TerminalId::new();
        let key = DeliveryKey::new_v4();
        deliveries
            .ledger
            .lock()
            .unwrap()
            .db
            .execute_batch(
                "CREATE TRIGGER fail_reservation BEFORE INSERT ON chat_deliveries
                 BEGIN SELECT RAISE(FAIL, 'fixture write failure'); END;",
            )
            .unwrap();

        assert_eq!(
            deliveries.deliver(chat, key, WAIT, unknown, || panic!("reservation failed")),
            Err(Rejection::Refused(DELIVERY_NOT_SENT.into())),
        );
        deliveries
            .ledger
            .lock()
            .unwrap()
            .db
            .execute_batch("DROP TRIGGER fail_reservation")
            .unwrap();
        let (acknowledge, admit) = admitted();
        acknowledge.send(Ok(())).unwrap();
        assert_eq!(deliveries.deliver(chat, key, WAIT, unknown, admit), Ok(()));
    }

    #[test]
    fn restoration_preserves_disk_history_and_deliveries_made_in_memory() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("deliveries.sqlite3");
        let disk = Deliveries::at(&path).unwrap();
        let chat = TerminalId::new();
        let on_disk = DeliveryKey::new_v4();
        let in_memory = DeliveryKey::new_v4();
        let in_flight = DeliveryKey::new_v4();
        let (acknowledge, admit) = admitted();
        acknowledge.send(Ok(())).unwrap();
        assert_eq!(disk.deliver(chat, on_disk, WAIT, unknown, admit), Ok(()));

        let fallback = Deliveries::in_memory().unwrap();
        assert!(
            fallback.restore_at(&path).is_err(),
            "the disk ledger is busy"
        );
        let (acknowledge, admit) = admitted();
        acknowledge.send(Ok(())).unwrap();
        assert_eq!(
            fallback.deliver(chat, in_memory, WAIT, unknown, admit),
            Ok(())
        );
        let (late, admit) = admitted();
        assert_eq!(
            fallback.deliver(chat, in_flight, Duration::ZERO, unknown, admit),
            Err(unknown()),
        );
        drop(disk);
        fallback.restore_at(&path).unwrap();
        let do_not_send = || panic!("the delivery already reached the provider");
        assert_eq!(
            fallback.deliver(chat, in_flight, Duration::ZERO, unknown, do_not_send),
            Err(unknown()),
        );
        late.send(Ok(())).unwrap();
        assert_eq!(
            fallback.deliver(chat, in_flight, WAIT, unknown, do_not_send),
            Ok(())
        );
        drop(fallback);

        let reopened = Deliveries::at(&path).unwrap();
        for key in [on_disk, in_memory, in_flight] {
            assert_eq!(
                reopened.deliver(chat, key, WAIT, unknown, do_not_send),
                Ok(())
            );
        }
    }

    #[test]
    fn a_late_fallback_refusal_cannot_erase_an_earlier_disk_receipt() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("deliveries.sqlite3");
        let disk = Deliveries::at(&path).unwrap();
        let chat = TerminalId::new();
        let key = DeliveryKey::new_v4();
        let (acknowledge, admit) = admitted();
        acknowledge.send(Ok(())).unwrap();
        assert_eq!(disk.deliver(chat, key, WAIT, unknown, admit), Ok(()));
        drop(disk);

        let fallback = Deliveries::in_memory().unwrap();
        let (late, admit) = admitted();
        assert_eq!(
            fallback.deliver(chat, key, Duration::ZERO, unknown, admit),
            Err(unknown()),
        );
        fallback.restore_at(&path).unwrap();
        late.send(Err(Rejection::Refused("busy".into()))).unwrap();
        assert_eq!(
            fallback.deliver(chat, key, WAIT, unknown, || panic!("disk receipt was lost")),
            Ok(()),
        );
    }

    #[test]
    fn disk_ledger_uses_versioned_schema_and_wal_with_normal_sync() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("deliveries.sqlite3");
        let deliveries = Deliveries::at(&path).unwrap();
        {
            let ledger = deliveries.ledger.lock().unwrap();
            let db = &ledger.db;
            let mode: String = db
                .pragma_query_value(None, "journal_mode", |row| row.get(0))
                .unwrap();
            let sync: i64 = db
                .pragma_query_value(None, "synchronous", |row| row.get(0))
                .unwrap();
            let version: u32 = db
                .pragma_query_value(None, "user_version", |row| row.get(0))
                .unwrap();
            assert_eq!((mode.as_str(), sync, version), ("wal", 1, SCHEMA_VERSION));
            db.pragma_update(None, "user_version", SCHEMA_VERSION + 1)
                .unwrap();
        }
        drop(deliveries);
        assert!(Deliveries::at(&path).is_err());
        let db = Connection::open(&path).unwrap();
        let version: u32 = db
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION + 1);
    }

    #[test]
    fn a_reopened_ledger_remembers_outcomes_and_reservations() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("deliveries.sqlite3");
        let chat = TerminalId::new();
        let accepted = DeliveryKey::new_v4();
        let pending = DeliveryKey::new_v4();
        let refused = DeliveryKey::new_v4();
        {
            let deliveries = Deliveries::at(&path).unwrap();
            assert!(
                Deliveries::at(&path).is_err(),
                "only the owner may settle reservations"
            );
            let (acknowledge, admit) = admitted();
            acknowledge.send(Ok(())).unwrap();
            assert_eq!(
                deliveries.deliver(chat, accepted, WAIT, unknown, admit),
                Ok(())
            );
            let (_acknowledge, admit) = admitted();
            assert_eq!(
                deliveries.deliver(chat, pending, WAIT, unknown, admit),
                Err(unknown())
            );
            let refuse = || Err(Rejection::Refused("busy".into()));
            assert_eq!(
                deliveries.deliver(chat, refused, WAIT, unknown, refuse),
                Err(Rejection::Refused("busy".into()))
            );
        }
        let deliveries = Deliveries::at(&path).unwrap();
        let do_not_send = || panic!("an earlier send may have reached the provider");
        assert_eq!(
            deliveries.deliver(chat, accepted, WAIT, unknown, do_not_send),
            Ok(())
        );
        assert_eq!(
            deliveries.deliver(chat, pending, WAIT, unknown, do_not_send),
            Err(Rejection::Unacknowledged(LOST_DELIVERY_OUTCOME.into())),
        );
        let (acknowledge, admit) = admitted();
        acknowledge.send(Ok(())).unwrap();
        assert_eq!(
            deliveries.deliver(chat, refused, WAIT, unknown, admit),
            Ok(())
        );
        // Each conversation has its own delivery keys.
        let (acknowledge, admit) = admitted();
        acknowledge.send(Ok(())).unwrap();
        assert_eq!(
            deliveries.deliver(TerminalId::new(), accepted, WAIT, unknown, admit),
            Ok(())
        );
    }
}
