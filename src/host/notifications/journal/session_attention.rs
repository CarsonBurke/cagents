use super::{Journal, notices, owners, seen};
use crate::notifications::{Event, Notice, Target};
use diesel::{connection::SimpleConnection, prelude::*, sqlite::SqliteConnection};
use uuid::Uuid;

use super::super::session_attention::Clear;

diesel::table! {
    session_attention_cursors (actor) {
        actor -> Text,
        sequence -> BigInt,
    }
}

pub(super) fn migrate(connection: &mut SqliteConnection) -> Result<(), String> {
    connection
        .batch_execute("CREATE TABLE IF NOT EXISTS session_attention_cursors (actor TEXT PRIMARY KEY, sequence INTEGER NOT NULL)")
        .map_err(|error| error.to_string())
}

impl Journal {
    pub(in crate::host::notifications) fn notice_for_target(
        &mut self,
        target: &Target,
    ) -> Result<Option<(Notice, i64)>, String> {
        notice_for_key(&mut self.0, &target.storage_key()).map_err(|error| error.to_string())
    }

    pub(in crate::host::notifications) fn clear_session_notices(
        &mut self,
        targets: impl IntoIterator<Item = Target>,
        clear: Clear,
        cursor: Option<(Uuid, u64)>,
    ) -> Result<Vec<Notice>, String> {
        let cursor = cursor
            .map(|(actor, sequence)| i64::try_from(sequence).map(|sequence| (actor, sequence)))
            .transpose()
            .map_err(|error| error.to_string())?;
        self.0
            .transaction::<_, diesel::result::Error, _>(|connection| {
                let mut cleared = Vec::new();
                if let Some((actor, sequence)) = cursor {
                    use session_attention_cursors::dsl as cursors;
                    let actor = actor.to_string();
                    // The helper may replay a page after a later outcome commits.
                    let stored = cursors::session_attention_cursors
                        .find(&actor)
                        .select(cursors::sequence)
                        .first::<i64>(connection)
                        .optional()?;
                    if stored.is_some_and(|stored| stored >= sequence) {
                        return Ok(cleared);
                    }
                    diesel::insert_into(cursors::session_attention_cursors)
                        .values((cursors::actor.eq(&actor), cursors::sequence.eq(sequence)))
                        .on_conflict(cursors::actor)
                        .do_update()
                        .set(cursors::sequence.eq(sequence))
                        .execute(connection)?;
                }
                for target in targets {
                    let key = target.storage_key();
                    let Some((notice, ordinal)) = notice_for_key(connection, &key)? else {
                        continue;
                    };
                    if notice.acknowledged || !clear.matches(&notice, ordinal) {
                        continue;
                    }
                    diesel::update(notices::table.find(&key))
                        .set((notices::acknowledged.eq(true), notices::delivered.eq(true)))
                        .execute(connection)?;
                    cleared.push(notice);
                }
                Ok(cleared)
            })
            .map_err(|error| error.to_string())
    }

    pub(in crate::host::notifications) fn retire_session_notices(
        &mut self,
        targets: Vec<(Uuid, Target)>,
    ) -> Result<Vec<Notice>, String> {
        // Admission fences new references until this transaction commits.
        // Defer on an external SQLite writer instead of holding that fence.
        self.0
            .batch_execute("PRAGMA busy_timeout=0;")
            .map_err(|error| error.to_string())?;
        let retired = self
            .0
            .immediate_transaction::<_, diesel::result::Error, _>(|connection| {
                let mut retired = Vec::new();
                for (id, target) in targets {
                    let key = target.storage_key();
                    if let Some((notice, _)) = notice_for_key(connection, &key)?
                        && notice.id == id
                    {
                        diesel::delete(notices::table.find(&key)).execute(connection)?;
                        diesel::delete(seen::table.filter(seen::session.eq(&key)))
                            .execute(connection)?;
                        diesel::delete(owners::table.find(&key)).execute(connection)?;
                        retired.push(notice);
                    }
                }
                Ok(retired)
            });
        self.0
            .batch_execute("PRAGMA busy_timeout=1000;")
            .map_err(|error| error.to_string())?;
        retired.map_err(|error| error.to_string())
    }
}

fn notice_for_key(
    connection: &mut SqliteConnection,
    key: &str,
) -> Result<Option<(Notice, i64)>, diesel::result::Error> {
    let row = notices::table
        .find(key)
        .select((
            notices::id,
            notices::event,
            notices::delivered,
            notices::acknowledged,
            notices::ordinal,
        ))
        .first::<(String, String, bool, bool, i64)>(connection)
        .optional()?;
    row.map(|(id, event, delivered, acknowledged, ordinal)| {
        Ok((
            Notice {
                id: id.parse::<Uuid>().map_err(|error| {
                    diesel::result::Error::DeserializationError(Box::new(error))
                })?,
                event: Event::from_journal(&event).map_err(|error| {
                    diesel::result::Error::DeserializationError(Box::new(error))
                })?,
                delivered,
                acknowledged,
                window: None,
            },
            ordinal,
        ))
    })
    .transpose()
}
