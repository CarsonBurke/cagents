use rusqlite::{OptionalExtension, TransactionBehavior, params};

use super::{StateStore, ensure_open};
use crate::{
    github::PullRequestKey,
    pull_request_review::cache::{self, Snapshot, Store},
};

// Keep review-only file metadata beside the existing flattened diff entry.
// Old cache rows have no badge field and remain readable without a schema bump.
#[derive(serde::Serialize, serde::Deserialize)]
struct StoredEntry {
    #[serde(flatten)]
    entry: crate::repository::DiffEntry,
    #[serde(default)]
    changed_since_viewed: bool,
}

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
            let stored:StoredEntry=serde_json::from_str(&entry).map_err(|error|error.to_string())?;
            snapshot.files.push(cache::File {
                entry: stored.entry,
                changed_since_viewed: stored.changed_since_viewed,
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

    fn save(&self, snapshot: &Snapshot, order: &cache::WriteOrder) -> Result<(), String> {
        snapshot.validate()?;
        let repository = snapshot.key.repository.gh_selector().to_ascii_lowercase();
        let entries = snapshot
            .files
            .iter()
            .map(|file| serde_json::to_string(&StoredEntry{entry:file.entry.clone(),changed_since_viewed:file.changed_since_viewed}))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())?;
        let mut inner = self.lock_inner().map_err(|error| error.to_string())?;
        ensure_open(&inner).map_err(|error| error.to_string())?;
        let tx = inner
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        let previous: Option<(String, u64, String)> = tx.query_row(
            "SELECT write_epoch, write_order, record FROM review_cache WHERE repository = ?1 AND number = ?2",
            params![repository, snapshot.key.number], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        ).optional().map_err(|error| error.to_string())?;
        if previous
            .as_ref()
            .is_some_and(|(boot, previous, _)| order.superseded_by(boot, *previous))
        {
            return if snapshot.drafts_changed {
                Err("A newer local review save finished first. Your draft is not saved yet; retry saving it.".into())
            } else {
                Ok(())
            };
        }
        let mut metadata = snapshot.clone();
        metadata.files.clear();
        if let Some((_, _, record)) = previous {
            let old: Snapshot = serde_json::from_str(&record).map_err(|error| error.to_string())?;
            metadata.merge_drafts(&old)?;
        }
        metadata.drafts_basis = None;
        let record = serde_json::to_string(&metadata).map_err(|error| error.to_string())?;
        let byte_size =
            record.len() as u64 + entries.iter().map(|entry| entry.len() as u64).sum::<u64>();
        if byte_size > cache::MAX_CACHE_BYTES {
            return Err("Cached review exceeds the size limit".into());
        }
        // Reinsert an accepted snapshot at the newest rowid while holding the
        // transaction. The parent deletion cascades its old file rows; neither
        // readers nor competing writers can observe an intermediate absence.
        // Recency follows commits and remains independent of wall-clock jumps.
        tx.execute(
            "DELETE FROM review_cache WHERE repository = ?1 AND number = ?2",
            params![repository, snapshot.key.number],
        )
        .map_err(|error| error.to_string())?;
        // SQLite chooses a random rowid when the largest one is i64::MAX.
        // Allocate explicitly so an exhausted/imported database cannot make a
        // fresh record appear older. Failure rolls the entire transaction back.
        let latest: i64 = tx
            .query_row(
                "SELECT coalesce(max(rowid), 0) FROM review_cache",
                [],
                |row| row.get(0),
            )
            .map_err(|error| error.to_string())?;
        let recency = latest
            .checked_add(1)
            .ok_or("Review-cache recency is exhausted")?;
        tx.execute(
            "INSERT INTO review_cache(rowid, repository, number, head, viewer, record, saved_ms, write_epoch, write_order, byte_size)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![recency, repository, snapshot.key.number, snapshot.range.head.as_str(), snapshot.viewer, record, snapshot.saved_ms, order.epoch.as_str(), order.tick, byte_size],
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
        let records: Vec<(i64, u64, bool)> = {
            let mut statement = tx
                .prepare("SELECT rowid, byte_size, record FROM review_cache ORDER BY rowid ASC")
                .map_err(|error| error.to_string())?;
            let rows = statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, u64>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })
                .map_err(|error| error.to_string())?;
            rows.map(|row| {
                let (id, bytes, record) = row.map_err(|error| error.to_string())?;
                let snapshot: Snapshot =
                    serde_json::from_str(&record).map_err(|error| error.to_string())?;
                Ok((id, bytes, snapshot.drafts.is_some()))
            })
            .collect::<Result<_, String>>()?
        };
        let mut count = records.len();
        let mut bytes = records.iter().map(|(_, bytes, _)| *bytes).sum::<u64>();
        for (id, size, has_draft) in records {
            if count <= cache::MAX_CACHED_REVIEWS && bytes <= cache::MAX_CACHE_BYTES {
                break;
            }
            if has_draft {
                continue;
            }
            tx.execute("DELETE FROM review_cache WHERE rowid = ?1", params![id])
                .map_err(|error| error.to_string())?;
            count -= 1;
            bytes = bytes.saturating_sub(size);
        }
        if count > cache::MAX_CACHED_REVIEWS || bytes > cache::MAX_CACHE_BYTES {
            return Err("Local review storage is full of saved drafts. Saved text was kept; finish or cancel a saved draft before saving another.".into());
        }
        tx.commit().map_err(|error| error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changed_since_viewed_survives_sqlite_and_legacy_rows_default_to_false() {
        let directory=tempfile::tempdir().unwrap();
        let store=StateStore::open(directory.path().join("state.sqlite3"),crate::model::BootId::new("boot")).unwrap();
        let mut snapshot=cache::fixture();snapshot.files[0].changed_since_viewed=true;
        store.save(&snapshot,&cache::WriteOrder::fixture(1)).unwrap();
        assert!(store.load(&snapshot.key).unwrap().unwrap().files[0].changed_since_viewed);
        let legacy=serde_json::to_string(&snapshot.files[0].entry).unwrap();
        store.lock_inner().unwrap().connection.execute("UPDATE review_cache_files SET entry = ?1",params![legacy]).unwrap();
        assert!(!store.load(&snapshot.key).unwrap().unwrap().files[0].changed_since_viewed);
    }

    #[test]
    fn sqlite_recovery_text_has_order_conflict_and_eviction_protection() {
        let directory = tempfile::tempdir().unwrap();
        let store = StateStore::open(
            directory.path().join("state.sqlite3"),
            crate::model::BootId::new("boot"),
        )
        .unwrap();
        cache::assert_draft_protection(&store);
    }

    #[test]
    fn unrelated_snapshot_preserves_draft_but_explicit_local_cancel_clears_it() {
        let directory = tempfile::tempdir().unwrap();
        let store = StateStore::open(
            directory.path().join("state.sqlite3"),
            crate::model::BootId::new("boot"),
        )
        .unwrap();
        let mut draft = cache::fixture();
        draft.drafts = Some(cache::Drafts {
            composer: None,
            summary: "Keep my draft".into(),
            event: crate::github::ReviewEvent::Comment,
            unknown: Some("unresolved receipt".into()),
        });
        draft.drafts_changed = true;
        store.save(&draft, &cache::WriteOrder::fixture(1)).unwrap();
        let mut background = cache::fixture();
        background.files[0].viewed = false;
        store
            .save(&background, &cache::WriteOrder::fixture(2))
            .unwrap();
        let restored = store.load(&background.key).unwrap().unwrap();
        assert_eq!(restored.drafts, draft.drafts);
        assert!(!restored.files[0].viewed);
        background.drafts_changed = true;
        store
            .save(&background, &cache::WriteOrder::fixture(3))
            .unwrap();
        assert!(
            store
                .load(&background.key)
                .unwrap()
                .unwrap()
                .drafts
                .is_none()
        );
    }

    #[test]
    fn sqlite_eviction_tracks_commits_and_repeated_updates_after_clock_rollback() {
        let directory = tempfile::tempdir().unwrap();
        let store = StateStore::open(
            directory.path().join("state.sqlite3"),
            crate::model::BootId::new("boot"),
        )
        .unwrap();
        let snapshot_for = |number| {
            let mut snapshot = cache::fixture();
            snapshot.key.number = number;
            snapshot.range.number = number;
            snapshot.pull_request = crate::pull_request_review::tests::pull_request(number);
            snapshot.saved_ms = 10_000 + number;
            snapshot
        };
        for number in 1..=32 {
            store
                .save(&snapshot_for(number), &cache::WriteOrder::fixture(number))
                .unwrap();
        }
        let mut refreshed = snapshot_for(1);
        refreshed.saved_ms = 1;
        refreshed.files[0].viewed = false;
        store
            .save(&refreshed, &cache::WriteOrder::fixture(100))
            .unwrap();
        let mut latest = snapshot_for(33);
        latest.saved_ms = 0;
        for order in 101..=103 {
            latest.files[0].viewed = order % 2 == 0;
            store
                .save(&latest, &cache::WriteOrder::fixture(order))
                .unwrap();
            assert_eq!(store.load(&latest.key).unwrap(), Some(latest.clone()));
        }
        assert_eq!(store.load(&refreshed.key).unwrap(), Some(refreshed));
        assert!(
            store.load(&snapshot_for(2).key).unwrap().is_none(),
            "least recent commit is evicted despite future wall time"
        );
        assert!(store.load(&snapshot_for(3).key).unwrap().is_some());
        let inner = store.lock_inner().unwrap();
        assert_eq!(
            inner
                .connection
                .query_row("SELECT count(*) FROM review_cache", [], |row| row
                    .get::<_, u64>(0))
                .unwrap(),
            32
        );
        assert_eq!(
            inner
                .connection
                .query_row("SELECT count(*) FROM review_cache_files", [], |row| row
                    .get::<_, u64>(0))
                .unwrap(),
            32
        );
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
    fn exhausted_sqlite_cache_recency_preserves_existing_snapshot_atomically() {
        let directory = tempfile::tempdir().unwrap();
        let store = StateStore::open(
            directory.path().join("state.sqlite3"),
            crate::model::BootId::new("boot"),
        )
        .unwrap();
        let original = cache::fixture();
        store
            .save(&original, &cache::WriteOrder::fixture(1))
            .unwrap();
        store
            .lock_inner()
            .unwrap()
            .connection
            .execute("UPDATE review_cache SET rowid = ?1", params![i64::MAX])
            .unwrap();
        let mut other = original.clone();
        other.key.number = 2;
        other.range.number = 2;
        other.pull_request = crate::pull_request_review::tests::pull_request(2);
        assert!(store.save(&other, &cache::WriteOrder::fixture(2)).is_err());
        assert_eq!(store.load(&original.key).unwrap(), Some(original.clone()));
        assert!(store.load(&other.key).unwrap().is_none());
        // Refreshing the only record removes the exhausted rowid in the same
        // transaction and can safely restart recency from one.
        other = original;
        other.files[0].viewed = false;
        store.save(&other, &cache::WriteOrder::fixture(3)).unwrap();
        assert_eq!(store.load(&other.key).unwrap(), Some(other));
    }

    #[test]
    fn cache_keeps_unfetched_viewed_states_distinct_from_unviewed() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::open(
            &dir.path().join("state.sqlite3"),
            crate::model::BootId::new("boot"),
        )
        .unwrap();
        let mut snapshot = cache::fixture();
        snapshot
            .unknown_paths
            .insert(snapshot.files[0].entry.path.clone());
        snapshot.files[0].viewed = false;
        snapshot.files[0].unverified = true;
        store
            .save(&snapshot, &cache::WriteOrder::fixture(1))
            .unwrap();
        assert_eq!(store.load(&snapshot.key).unwrap(), Some(snapshot));
    }

    #[test]
    fn version_eight_migrates_and_eviction_cascades_file_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.sqlite3");
        {
            let store = StateStore::open(&path, crate::model::BootId::new("boot")).unwrap();
            store
                .lock_inner()
                .unwrap()
                .connection
                .execute_batch(
                    "DROP TABLE review_cache_files; DROP TABLE review_cache;
                 ALTER TABLE terminal_recipes DROP COLUMN predecessor; PRAGMA user_version = 8;",
                )
                .unwrap();
        }
        let store = StateStore::open(&path, crate::model::BootId::new("boot")).unwrap();
        assert_eq!(
            store.schema_version().unwrap(),
            crate::persistence::CURRENT_SCHEMA_VERSION
        );
        for number in 1..=33 {
            let mut snapshot = cache::fixture();
            snapshot.key.number = number;
            snapshot.range.number = number;
            snapshot.pull_request = crate::pull_request_review::tests::pull_request(number);
            snapshot.saved_ms = number;
            store
                .save(&snapshot, &cache::WriteOrder::fixture(number))
                .unwrap();
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
    fn sqlite_cache_accepts_legacy_write_epoch_after_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.sqlite3");
        let mut snapshot = cache::fixture();
        {
            let store = StateStore::open(&path, crate::model::BootId::new("boot")).unwrap();
            store
                .save(&snapshot, &cache::WriteOrder::fixture(5))
                .unwrap();
            let saved = snapshot.clone();
            snapshot.files[0].viewed = false;
            store
                .save(&snapshot, &cache::WriteOrder::fixture(4))
                .unwrap();
            assert_eq!(store.load(&snapshot.key).unwrap(), Some(saved));
            // Model the process-scoped epoch written by an older application.
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
        store
            .save(&snapshot, &cache::WriteOrder::fixture(1))
            .unwrap();
        assert_eq!(store.load(&snapshot.key).unwrap(), Some(snapshot));
    }
}
