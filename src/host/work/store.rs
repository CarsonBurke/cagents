//! The Work board's own durable facts, independent of the workspace model:
//! when an agent last worked in each worktree, the user's pins, the last forge
//! listing per repository and the Linear tickets cards name.
//! Everything else on a card is re-read from Git.
use std::{
    collections::{BTreeMap, HashMap},
    ffi::OsStr,
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
};

use diesel::{connection::SimpleConnection, prelude::*, sqlite::SqliteConnection};

use crate::work::Column;

const SCHEMA_VERSION: i32 = 1;

mod schema {
    diesel::table! {
        session_activity (worktree) {
            worktree -> Binary,
            at_unix_ms -> BigInt,
        }
    }
    diesel::table! {
        pins (repository, item) {
            repository -> Binary,
            item -> Text,
            placement -> Text,
        }
    }
    diesel::table! {
        forge (repository) {
            repository -> Binary,
            synced_at_unix_ms -> BigInt,
            snapshot -> Text,
        }
    }
    diesel::table! {
        tickets (reference) {
            reference -> Text,
            identifier -> Text,
            title -> Text,
            url -> Text,
        }
    }
}
use schema::{forge, pins, session_activity, tickets};

/// What a Linear reference, a linked issue's id or a branch name's
/// identifier, resolved to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Ticket {
    pub identifier: String,
    pub title: String,
    pub url: String,
}

#[derive(diesel::QueryableByName)]
struct SchemaVersion {
    #[diesel(sql_type = diesel::sql_types::Integer)]
    user_version: i32,
}

pub(super) struct Store(SqliteConnection);

fn bytes(path: &Path) -> &[u8] {
    path.as_os_str().as_bytes()
}

fn path(bytes: Vec<u8>) -> PathBuf {
    PathBuf::from(OsStr::from_bytes(&bytes))
}

fn millis(value: i64) -> u64 {
    u64::try_from(value).unwrap_or_default()
}

fn stored(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn failed(error: impl std::fmt::Display) -> String {
    format!("Work board store: {error}")
}

impl Store {
    pub(super) fn open(path: &Path) -> Result<Self, String> {
        drop(crate::secure_file::open_private_append_file(path).map_err(failed)?);
        // A URI keeps non-UTF-8 names intact and quotes SQLite's URI syntax.
        let absolute = std::path::absolute(path).map_err(failed)?;
        let uri = url::Url::from_file_path(absolute)
            .map_err(|()| failed("the path cannot be represented as a file URI"))?;
        let mut connection = SqliteConnection::establish(uri.as_str()).map_err(failed)?;
        let version: SchemaVersion = diesel::sql_query("PRAGMA user_version")
            .get_result(&mut connection)
            .map_err(failed)?;
        if version.user_version > SCHEMA_VERSION {
            return Err(failed("it was created by a newer cagents version"));
        }
        // Losing the last write to a power cut costs at most one pin or one
        // activity stamp; every other fact is re-read from Git.
        connection
            .batch_execute(
                "PRAGMA busy_timeout=1000;
                 PRAGMA journal_mode=WAL;
                 PRAGMA synchronous=NORMAL;
                 CREATE TABLE IF NOT EXISTS session_activity (
                     worktree BLOB PRIMARY KEY, at_unix_ms INTEGER NOT NULL);
                 CREATE TABLE IF NOT EXISTS pins (
                     repository BLOB NOT NULL, item TEXT NOT NULL, placement TEXT NOT NULL,
                     PRIMARY KEY (repository, item));
                 CREATE TABLE IF NOT EXISTS forge (
                     repository BLOB PRIMARY KEY, synced_at_unix_ms INTEGER NOT NULL,
                     snapshot TEXT NOT NULL);
                 CREATE TABLE IF NOT EXISTS tickets (
                     reference TEXT PRIMARY KEY, identifier TEXT NOT NULL, title TEXT NOT NULL,
                     url TEXT NOT NULL);
                 PRAGMA user_version=1;",
            )
            .map_err(failed)?;
        Ok(Self(connection))
    }

    /// When an agent last worked in each worktree.
    pub(super) fn session_activity(&mut self) -> Result<HashMap<PathBuf, u64>, String> {
        Ok(session_activity::table
            .select((session_activity::worktree, session_activity::at_unix_ms))
            .load::<(Vec<u8>, i64)>(&mut self.0)
            .map_err(failed)?
            .into_iter()
            .map(|(worktree, at)| (path(worktree), millis(at)))
            .collect())
    }

    /// Records activity stamps, never moving one backwards, and forgets those
    /// older than `forget_before_unix_ms`: past every stale window, a stamp no
    /// longer changes any card.
    pub(super) fn record_session_activity(
        &mut self,
        stamps: &[(PathBuf, u64)],
        forget_before_unix_ms: u64,
    ) -> Result<(), String> {
        self.0
            .transaction::<_, diesel::result::Error, _>(|connection| {
                for (worktree, at) in stamps {
                    diesel::sql_query(
                        "INSERT INTO session_activity (worktree, at_unix_ms) VALUES (?, ?)
                         ON CONFLICT (worktree) DO UPDATE
                         SET at_unix_ms = max(at_unix_ms, excluded.at_unix_ms)",
                    )
                    .bind::<diesel::sql_types::Binary, _>(bytes(worktree))
                    .bind::<diesel::sql_types::BigInt, _>(stored(*at))
                    .execute(connection)?;
                }
                diesel::delete(
                    session_activity::table
                        .filter(session_activity::at_unix_ms.lt(stored(forget_before_unix_ms))),
                )
                .execute(connection)?;
                Ok(())
            })
            .map_err(failed)
    }

    /// Every pin, by repository and item storage key.
    pub(super) fn pins(&mut self) -> Result<BTreeMap<(PathBuf, String), Column>, String> {
        let rows = pins::table
            .select((pins::repository, pins::item, pins::placement))
            .load::<(Vec<u8>, String, String)>(&mut self.0)
            .map_err(failed)?;
        // A column a newer version added is ignored rather than failing the
        // whole board; the pin stays stored for that version.
        Ok(rows
            .into_iter()
            .filter_map(|(repository, item, placement)| {
                Some(((path(repository), item), placement.parse().ok()?))
            })
            .collect())
    }

    pub(super) fn set_pin(
        &mut self,
        repository: &Path,
        item: &str,
        column: Option<Column>,
    ) -> Result<(), String> {
        let key = (pins::repository.eq(bytes(repository)), pins::item.eq(item));
        match column {
            Some(column) => diesel::insert_into(pins::table)
                .values((key.0, key.1, pins::placement.eq(column.as_str())))
                .on_conflict((pins::repository, pins::item))
                .do_update()
                .set(pins::placement.eq(column.as_str()))
                .execute(&mut self.0),
            None => diesel::delete(pins::table.filter(key.0).filter(key.1)).execute(&mut self.0),
        }
        .map(drop)
        .map_err(failed)
    }

    /// Forgets the pins of cards that no longer exist in `repository`, so a
    /// branch recreated under an old name does not inherit an old placement.
    pub(super) fn retain_pins(
        &mut self,
        repository: &Path,
        keep: impl Fn(&str) -> bool,
    ) -> Result<Vec<String>, String> {
        let items = pins::table
            .filter(pins::repository.eq(bytes(repository)))
            .select(pins::item)
            .load::<String>(&mut self.0)
            .map_err(failed)?;
        let gone = items
            .into_iter()
            .filter(|item| !keep(item))
            .collect::<Vec<_>>();
        if !gone.is_empty() {
            diesel::delete(
                pins::table
                    .filter(pins::repository.eq(bytes(repository)))
                    .filter(pins::item.eq_any(&gone)),
            )
            .execute(&mut self.0)
            .map_err(failed)?;
        }
        Ok(gone)
    }

    /// The last forge listing of each repository: when it was taken and its
    /// serialized form, which the forge poller owns.
    pub(super) fn forge_snapshots(&mut self) -> Result<BTreeMap<PathBuf, (u64, String)>, String> {
        Ok(forge::table
            .select((forge::repository, forge::synced_at_unix_ms, forge::snapshot))
            .load::<(Vec<u8>, i64, String)>(&mut self.0)
            .map_err(failed)?
            .into_iter()
            .map(|(repository, at, snapshot)| (path(repository), (millis(at), snapshot)))
            .collect())
    }

    pub(super) fn save_forge_snapshot(
        &mut self,
        repository: &Path,
        synced_at_unix_ms: u64,
        snapshot: &str,
    ) -> Result<(), String> {
        diesel::insert_into(forge::table)
            .values((
                forge::repository.eq(bytes(repository)),
                forge::synced_at_unix_ms.eq(stored(synced_at_unix_ms)),
                forge::snapshot.eq(snapshot),
            ))
            .on_conflict(forge::repository)
            .do_update()
            .set((
                forge::synced_at_unix_ms.eq(stored(synced_at_unix_ms)),
                forge::snapshot.eq(snapshot),
            ))
            .execute(&mut self.0)
            .map(drop)
            .map_err(failed)
    }
}

impl Store {
    pub(super) fn tickets(&mut self) -> Result<BTreeMap<String, Ticket>, String> {
        Ok(tickets::table
            .select((
                tickets::reference,
                tickets::identifier,
                tickets::title,
                tickets::url,
            ))
            .load::<(String, String, String, String)>(&mut self.0)
            .map_err(failed)?
            .into_iter()
            .map(|(reference, identifier, title, url)| {
                (
                    reference,
                    Ticket {
                        identifier,
                        title,
                        url,
                    },
                )
            })
            .collect())
    }

    pub(super) fn save_ticket(&mut self, reference: &str, ticket: &Ticket) -> Result<(), String> {
        let values = (
            tickets::identifier.eq(&ticket.identifier),
            tickets::title.eq(&ticket.title),
            tickets::url.eq(&ticket.url),
        );
        diesel::insert_into(tickets::table)
            .values((tickets::reference.eq(reference), values))
            .on_conflict(tickets::reference)
            .do_update()
            .set(values)
            .execute(&mut self.0)
            .map(drop)
            .map_err(failed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn facts_survive_reopening_and_activity_never_moves_backwards() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("work.sqlite3");
        let repository = PathBuf::from(OsStr::from_bytes(b"/repo/\xff/.git"));
        let worktree = PathBuf::from("/repo/.worktrees/a");
        {
            let mut store = Store::open(&file).unwrap();
            store
                .record_session_activity(&[(worktree.clone(), 2_000)], 0)
                .unwrap();
            store
                .record_session_activity(&[(worktree.clone(), 1_000)], 0)
                .unwrap();
            store
                .set_pin(&repository, "branch:feature", Some(Column::InReview))
                .unwrap();
            store
                .set_pin(&repository, "branch:other", Some(Column::Stale))
                .unwrap();
            store
                .save_forge_snapshot(&repository, 5, "{\"first\":1}")
                .unwrap();
            store
                .save_forge_snapshot(&repository, 6, "{\"second\":2}")
                .unwrap();
        }
        let mut store = Store::open(&file).unwrap();
        assert_eq!(store.session_activity().unwrap()[&worktree], 2_000);
        assert_eq!(
            store.pins().unwrap(),
            BTreeMap::from([
                (
                    (repository.clone(), "branch:feature".to_owned()),
                    Column::InReview
                ),
                (
                    (repository.clone(), "branch:other".to_owned()),
                    Column::Stale
                ),
            ])
        );
        assert_eq!(
            store.forge_snapshots().unwrap()[&repository],
            (6, "{\"second\":2}".to_owned())
        );

        store.set_pin(&repository, "branch:other", None).unwrap();
        assert_eq!(
            store
                .retain_pins(&repository, |item| item != "branch:feature")
                .unwrap(),
            vec!["branch:feature".to_owned()]
        );
        assert!(store.pins().unwrap().is_empty());

        store.record_session_activity(&[], 3_000).unwrap();
        assert!(store.session_activity().unwrap().is_empty());

        let ticket = |identifier: &str| Ticket {
            identifier: identifier.into(),
            title: "Fix it".into(),
            url: format!("https://linear.app/team/issue/{identifier}"),
        };
        store.save_ticket("uuid", &ticket("MAIN-1")).unwrap();
        store.save_ticket("uuid", &ticket("CORE-2")).unwrap();
        assert_eq!(
            store.tickets().unwrap(),
            BTreeMap::from([("uuid".to_owned(), ticket("CORE-2"))])
        );
    }

    #[test]
    fn a_newer_schema_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("work.sqlite3");
        drop(Store::open(&file).unwrap());
        let mut connection = SqliteConnection::establish(file.to_str().unwrap()).unwrap();
        connection.batch_execute("PRAGMA user_version=2;").unwrap();
        drop(connection);
        assert!(
            Store::open(&file)
                .err()
                .unwrap()
                .contains("newer cagents version")
        );
    }
}
