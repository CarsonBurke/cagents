use rusqlite::{OptionalExtension, TransactionBehavior, params};

use super::{StateStore, ensure_open};
use crate::{
    github::PullRequestKey,
    pull_request_review::cache::{self, Snapshot, Store},
};

impl Store for StateStore {
    fn load(&self, key: &PullRequestKey) -> Result<Option<Snapshot>, String> {
        key.validate().map_err(|error| error.to_string())?;
        let repository = key.repository.gh_selector().to_ascii_lowercase();
        let mut inner = self.lock_inner().map_err(|error| error.to_string())?;
        ensure_open(&inner).map_err(|error| error.to_string())?;
        let tx = inner
            .connection
            .transaction()
            .map_err(|error| error.to_string())?;
        let record: Option<String> = tx
            .query_row(
                "SELECT record FROM review_cache WHERE repository = ?1 AND number = ?2",
                params![repository, key.number],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| error.to_string())?;
        let Some(record) = record else {
            return Ok(None);
        };
        if record.len() as u64 > cache::MAX_CACHE_BYTES {
            return Err("Cached review exceeds the size limit".into());
        }
        let mut snapshot: Snapshot =
            serde_json::from_str(&record).map_err(|error| error.to_string())?;
        let mut statement = tx
            .prepare(
                "SELECT entry, viewed, unverified FROM review_cache_files
             WHERE repository = ?1 AND number = ?2 AND head = ?3 ORDER BY position LIMIT ?4",
            )
            .map_err(|error| error.to_string())?;
        let mut rows = statement
            .query(params![
                repository,
                key.number,
                snapshot.range.head.as_str(),
                cache::MAX_CACHED_FILES + 1
            ])
            .map_err(|error| error.to_string())?;
        let mut bytes = record.len() as u64;
        while let Some(row) = rows.next().map_err(|error| error.to_string())? {
            let entry: String = row.get(0).map_err(|error| error.to_string())?;
            bytes = bytes.saturating_add(entry.len() as u64);
            if bytes > cache::MAX_CACHE_BYTES {
                return Err("Cached review exceeds the size limit".into());
            }
            snapshot.files.push(cache::File {
                entry: serde_json::from_str(&entry).map_err(|error| error.to_string())?,
                viewed: row.get(1).map_err(|error| error.to_string())?,
                unverified: row.get(2).map_err(|error| error.to_string())?,
            });
        }
        if !snapshot.key.repository.same_repository(&key.repository)
            || snapshot.key.number != key.number
        {
            return Err("Cached review belongs to a different repository".into());
        }
        snapshot.validate()?;
        Ok(Some(snapshot))
    }

    fn save(&self, snapshot: &Snapshot, order: u64) -> Result<(), String> {
        snapshot.validate()?;
        let repository = snapshot.key.repository.gh_selector().to_ascii_lowercase();
        let mut metadata = snapshot.clone();
        metadata.files.clear();
        let record = serde_json::to_string(&metadata).map_err(|error| error.to_string())?;
        let entries = snapshot
            .files
            .iter()
            .map(|file| serde_json::to_string(&file.entry))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())?;
        let byte_size =
            record.len() as u64 + entries.iter().map(|entry| entry.len() as u64).sum::<u64>();
        if byte_size > cache::MAX_CACHE_BYTES {
            return Err("Cached review exceeds the size limit".into());
        }
        let mut inner = self.lock_inner().map_err(|error| error.to_string())?;
        ensure_open(&inner).map_err(|error| error.to_string())?;
        let tx = inner
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        let previous: Option<(String, u64)> = tx.query_row(
            "SELECT write_epoch, write_order FROM review_cache WHERE repository = ?1 AND number = ?2",
            params![repository, snapshot.key.number], |row| Ok((row.get(0)?, row.get(1)?)),
        ).optional().map_err(|error| error.to_string())?;
        if previous.is_some_and(|(boot, previous)| {
            boot == self.review_cache_epoch.as_str() && previous >= order
        }) {
            return Ok(());
        }
        tx.execute(
            "DELETE FROM review_cache_files WHERE repository = ?1 AND number = ?2",
            params![repository, snapshot.key.number],
        )
        .map_err(|error| error.to_string())?;
        tx.execute(
            "INSERT INTO review_cache(repository, number, head, viewer, record, saved_ms, write_epoch, write_order, byte_size)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(repository, number) DO UPDATE SET head=excluded.head, viewer=excluded.viewer,
                 record=excluded.record, saved_ms=excluded.saved_ms, write_epoch=excluded.write_epoch, write_order=excluded.write_order, byte_size=excluded.byte_size",
            params![repository, snapshot.key.number, snapshot.range.head.as_str(), snapshot.viewer, record, snapshot.saved_ms, self.review_cache_epoch.as_str(), order, byte_size],
        ).map_err(|error| error.to_string())?;
        {
            let mut insert = tx.prepare(
                "INSERT INTO review_cache_files(repository, number, head, path, position, entry, viewed, unverified)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            ).map_err(|error| error.to_string())?;
            for (position, (file, entry)) in snapshot.files.iter().zip(entries).enumerate() {
                insert
                    .execute(params![
                        repository,
                        snapshot.key.number,
                        snapshot.range.head.as_str(),
                        file.entry.path.as_path().to_string_lossy(),
                        position,
                        entry,
                        file.viewed,
                        file.unverified
                    ])
                    .map_err(|error| error.to_string())?;
            }
        }
        tx.execute(
            "DELETE FROM review_cache WHERE rowid IN
                (SELECT rowid FROM (
                    SELECT rowid, row_number() OVER (ORDER BY saved_ms DESC, rowid DESC) AS rank,
                        sum(byte_size) OVER (ORDER BY saved_ms DESC, rowid DESC) AS bytes
                    FROM review_cache
                 ) WHERE rank > ?1 OR bytes > ?2)",
            params![cache::MAX_CACHED_REVIEWS, cache::MAX_CACHE_BYTES],
        )
        .map_err(|error| error.to_string())?;
        tx.commit().map_err(|error| error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_eight_migrates_and_eviction_cascades_file_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.sqlite3");
        {
            let store = StateStore::open(&path, crate::model::BootId::new("boot")).unwrap();
            store.lock_inner().unwrap().connection.execute_batch(
                "DROP TABLE review_cache_files; DROP TABLE review_cache; PRAGMA user_version = 8;",
            ).unwrap();
        }
        let store = StateStore::open(&path, crate::model::BootId::new("boot")).unwrap();
        assert_eq!(store.schema_version().unwrap(), 9);
        for number in 1..=33 {
            let mut snapshot = cache::fixture();
            snapshot.key.number = number;
            snapshot.range.number = number;
            snapshot.pull_request = crate::pull_request_review::tests::pull_request(number);
            snapshot.saved_ms = number;
            store.save(&snapshot, number).unwrap();
        }
        assert!(store.load(&cache::fixture().key).unwrap().is_none());
        let inner = store.lock_inner().unwrap();
        let count: u64 = inner
            .connection
            .query_row("SELECT count(*) FROM review_cache_files", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 32);
        assert_eq!(
            inner
                .connection
                .query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
                    row.get::<_, u64>(0)
                })
                .unwrap(),
            0
        );
    }

    #[test]
    fn sqlite_cache_reopens_with_a_new_write_epoch_on_the_same_boot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.sqlite3");
        let mut snapshot = cache::fixture();
        {
            let store = StateStore::open(&path, crate::model::BootId::new("boot")).unwrap();
            store.save(&snapshot, 5).unwrap();
            let saved = snapshot.clone();
            snapshot.files[0].viewed = false;
            store.save(&snapshot, 4).unwrap();
            assert_eq!(store.load(&snapshot.key).unwrap(), Some(saved));
            // Model a different UI process while keeping the OS boot ID.
            store
                .lock_inner()
                .unwrap()
                .connection
                .execute(
                    "UPDATE review_cache SET write_epoch = 'previous-process'",
                    [],
                )
                .unwrap();
        }
        let store = StateStore::open(&path, crate::model::BootId::new("boot")).unwrap();
        store.save(&snapshot, 1).unwrap();
        assert_eq!(store.load(&snapshot.key).unwrap(), Some(snapshot));
    }
}
