//! The daemon keeps recent delivery keys across actor replacement and restart.
//! Record each key before admitting work, then save the acknowledgement before
//! returning it to the sender. A crash can leave a reservation without an
//! acknowledgement. That key can never prove that its work was refused.

use super::Rejection;
use crate::{
    chat::{
        DeliveryKey,
        failure::delivery::{DELIVERY_HISTORY_UNAVAILABLE, LOST_DELIVERY_OUTCOME},
    },
    instance::OwnedLock,
    model::TerminalId,
};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use std::{collections::HashMap, path::Path, sync::Mutex, time::Duration};

pub(crate) type Reply = crossbeam_channel::Receiver<Result<(), Rejection>>;

const KEPT_PER_CHAT: i64 = 64;
const KEPT_TOTAL: i64 = 16_384;
const MAX_ERROR_BYTES: usize = 1024;
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
    Settled(Result<(), Rejection>),
}

struct State {
    db: Connection,
    pending: HashMap<Key, Pending>,
}

pub(crate) struct Deliveries {
    state: Mutex<State>,
    _lock: Option<OwnedLock>,
}

impl Deliveries {
    pub(crate) fn open() -> Result<Self, String> {
        let dirs = crate::instance::project_dirs().ok_or("the data directory is unavailable")?;
        Self::at(&dirs.data_dir().join("chat-deliveries.sqlite3"))
    }

    fn at(path: &Path) -> Result<Self, String> {
        drop(
            crate::secure_file::open_private_append_file(path)
                .map_err(|error| error.to_string())?,
        );
        // Daemons with distinct runtime directories can still share this data
        // directory. Hold its lock before settling old reservations.
        let lock = crate::instance::owned_lock(
            &path.with_extension("lock"),
            libc::LOCK_EX | libc::LOCK_NB,
        )
        .map_err(|error| error.to_string())?;
        let db = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )
        .map_err(|error| error.to_string())?;
        Self::initialize(db, Some(lock)).map_err(|error| error.to_string())
    }

    #[cfg(test)]
    fn in_memory() -> rusqlite::Result<Self> {
        Self::initialize(Connection::open_in_memory()?, None)
    }

    fn initialize(db: Connection, lock: Option<OwnedLock>) -> rusqlite::Result<Self> {
        db.execute_batch(
            "PRAGMA synchronous=FULL;
             CREATE TABLE IF NOT EXISTS chat_deliveries (
                 ordinal INTEGER PRIMARY KEY,
                 chat TEXT NOT NULL,
                 key TEXT NOT NULL,
                 settled INTEGER NOT NULL,
                 error TEXT,
                 UNIQUE(chat, key)
             );
             CREATE INDEX IF NOT EXISTS chat_deliveries_by_chat ON chat_deliveries(chat, ordinal);",
        )?;
        // Reservations left by the previous daemon did not receive a
        // durable final acknowledgement.
        db.execute(
            "UPDATE chat_deliveries SET settled=1, error=?1 WHERE settled=0",
            [LOST_DELIVERY_OUTCOME],
        )?;
        Ok(Self {
            state: Mutex::new(State {
                db,
                pending: HashMap::new(),
            }),
            _lock: lock,
        })
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
    ) -> Result<(), Rejection> {
        let key = Key(chat, key);
        let earlier = {
            let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
            state.collect_replies();
            if let Some(pending) = state.pending.get_mut(&key) {
                match pending {
                    Pending::Sending => return Err(unknown()),
                    Pending::Settled(_) => return Err(history_unavailable()),
                    Pending::Waiting(reply, _) => {
                        let reply = reply.clone();
                        *pending = Pending::Sending;
                        Some(reply)
                    }
                }
            } else {
                match state.reserve(key) {
                    Ok(Reservation::New) => {
                        state.pending.insert(key, Pending::Sending);
                        None
                    }
                    Ok(Reservation::Known(result)) => return result,
                    Ok(Reservation::Full) => return Err(Rejection::Refused(CROWDED.into())),
                    Err(error) => {
                        tracing::error!(%chat, %error, "cannot reserve a message delivery key");
                        return Err(history_unavailable());
                    }
                }
            }
        };

        let await_reply = |reply: Reply| match reply.recv_timeout(wait) {
            Ok(result) => Ok(result),
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => Err(reply),
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => Ok(Err(unknown())),
        };
        let outcome = match earlier.map(await_reply) {
            Some(Ok(Err(Rejection::Refused(_)))) | None => match admit() {
                Ok(reply) => await_reply(reply),
                Err(refusal) => Ok(Err(refusal)),
            },
            Some(outcome) => outcome,
        };

        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        match outcome {
            Err(reply) => {
                state.pending.insert(key, Pending::Waiting(reply, unknown));
                Err(unknown())
            }
            Ok(result) if State::finish(&state.db, key, &result) => {
                state.pending.remove(&key);
                result
            }
            Ok(result) => {
                // Retry saving the acknowledgement on the next request.
                // Until then the durable reservation still prevents replay.
                state.pending.insert(key, Pending::Settled(result));
                Err(history_unavailable())
            }
        }
    }
}

fn history_unavailable() -> Rejection {
    Rejection::Unacknowledged(DELIVERY_HISTORY_UNAVAILABLE.into())
}

enum Reservation {
    New,
    Known(Result<(), Rejection>),
    Full,
}

impl State {
    fn reserve(&mut self, key: Key) -> rusqlite::Result<Reservation> {
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
                (true, None) => Ok(()),
                (true, Some(error)) => Err(Rejection::Unacknowledged(error)),
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

    /// Collect late answers and retry storing any answer whose write failed.
    fn collect_replies(&mut self) {
        self.pending.retain(|&key, pending| {
            if let Pending::Waiting(reply, unknown) = pending {
                let result = match reply.try_recv() {
                    Ok(result) => result,
                    Err(crossbeam_channel::TryRecvError::Disconnected) => Err(unknown()),
                    Err(crossbeam_channel::TryRecvError::Empty) => return true,
                };
                *pending = Pending::Settled(result);
            }
            match pending {
                Pending::Settled(result) => !Self::finish(&self.db, key, result),
                Pending::Sending => true,
                Pending::Waiting(..) => unreachable!(),
            }
        });
    }

    fn finish(db: &Connection, key: Key, result: &Result<(), Rejection>) -> bool {
        match Self::save_outcome(db, key, result) {
            Ok(()) => true,
            Err(error) => {
                tracing::error!(chat = %key.0, %error, "cannot save a message acknowledgement");
                false
            }
        }
    }

    fn save_outcome(
        db: &Connection,
        key: Key,
        result: &Result<(), Rejection>,
    ) -> rusqlite::Result<()> {
        let (chat, delivery) = key.sql();
        let error = match result {
            Ok(()) => None,
            Err(Rejection::Unacknowledged(error)) => {
                Some(&error[..error.floor_char_boundary(MAX_ERROR_BYTES)])
            }
            Err(Rejection::Refused(_)) => {
                db.execute(
                    "DELETE FROM chat_deliveries WHERE chat=?1 AND key=?2",
                    params![chat, delivery],
                )?;
                return Ok(());
            }
        };
        db.execute(
            "UPDATE chat_deliveries SET settled=1, error=?3 WHERE chat=?1 AND key=?2",
            params![chat, delivery, error],
        )?;
        Ok(())
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
                .state
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
            .state
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
            .state
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
