//! Retained WAL space, not a bound on active transactions or pinned readers.
//! A checkpoint must never turn a committed event into a rejected/replayed one.
use rusqlite::{Connection, OpenFlags, TransactionState};
use std::{fs, io, os::unix::fs::MetadataExt, path::Path, time::Duration};

pub(super) const LIMIT: u64 = 1024 * 1024;

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Outcome {
    WithinLimit,
    DeferredTransaction,
    Checkpoint {
        busy: i64,
        log_frames: i64,
        checkpointed_frames: i64,
    },
    Unavailable,
}

// SQLite's Unix VFS compares its actual open database with the named file.
// Refuse maintenance if a moved/replaced cache no longer names that database.
fn still_named(db: &Connection) -> bool {
    let mut moved: std::ffi::c_int = 1;
    // SAFETY: db is borrowed for this synchronous call; the static database
    // name and writable integer have the types required by HAS_MOVED.
    let result = unsafe {
        rusqlite::ffi::sqlite3_file_control(
            db.handle(),
            c"main".as_ptr(),
            rusqlite::ffi::SQLITE_FCNTL_HAS_MOVED,
            (&mut moved as *mut std::ffi::c_int).cast(),
        )
    };
    result == rusqlite::ffi::SQLITE_OK && moved == 0
}

fn same_file(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    left.dev() == right.dev() && left.ino() == right.ino()
}

/// Called only while the Store retains its original private-cache lease.
/// There is one attempt, no busy wait, and no actor/Store reference escapes.
/// Busy/IO failures preserve both the reader and the original write result.
pub(super) fn after_write(main: &Connection, directory: &Path) -> Outcome {
    // An implicit SELECT can retain a read snapshot with autocommit still on.
    if !main.is_autocommit()
        || !matches!(
            main.transaction_state::<&str>(None),
            Ok(TransactionState::None)
        )
    {
        return Outcome::DeferredTransaction;
    }
    let database = directory.join("transcript.sqlite");
    let Some(original) = main.path().map(Path::new) else {
        return Outcome::Unavailable;
    };
    let Ok(named) = fs::symlink_metadata(&database) else {
        return Outcome::Unavailable;
    };
    let Ok(original_named) = fs::metadata(original) else {
        return Outcome::Unavailable;
    };
    if !named.is_file() || !same_file(&named, &original_named) || !still_named(main) {
        return Outcome::Unavailable;
    }
    match fs::symlink_metadata(directory.join("transcript.sqlite-wal")) {
        Ok(wal) if wal.is_file() && wal.len() <= LIMIT => return Outcome::WithinLimit,
        Ok(wal) if wal.is_file() => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Outcome::WithinLimit,
        _ => return Outcome::Unavailable,
    }

    let attempt = (|| -> rusqlite::Result<Outcome> {
        // A different connection preserves the main connection's actual busy
        // callback as well as its timeout. It is closed on every return path.
        let checkpoint = Connection::open_with_flags(
            &database,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_PRIVATE_CACHE
                | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        checkpoint.busy_timeout(Duration::ZERO)?;
        checkpoint.pragma_update(None, "cache_size", -64)?;
        checkpoint.pragma_update(None, "synchronous", "NORMAL")?;
        let mode: String = checkpoint.pragma_query_value(None, "journal_mode", |r| r.get(0))?;
        let current = fs::symlink_metadata(&database).ok();
        if mode != "wal"
            || !still_named(main)
            || !still_named(&checkpoint)
            || !current.is_some_and(|current| current.is_file() && same_file(&named, &current))
        {
            return Ok(Outcome::Unavailable);
        }
        // TRUNCATE can return BUSY as a successful SQL result row. Consume all
        // three fields; only (0,0,0) proves that the WAL was truncated to zero.
        checkpoint.query_row("PRAGMA main.wal_checkpoint(TRUNCATE)", [], |row| {
            Ok(Outcome::Checkpoint {
                busy: row.get(0)?,
                log_frames: row.get(1)?,
                checkpointed_frames: row.get(2)?,
            })
        })
    })();
    attempt.unwrap_or(Outcome::Unavailable)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::runtime::store::Store;
    use crate::chat::{Event, Item, ItemContent, Phase, Role};

    fn prose(id: &str, text: String) -> Item {
        Item {
            id: id.into(),
            role: Role::Assistant,
            started_at: None,
            at: None,
            content: ItemContent::Text {
                text,
                thinking: false,
                phase: Phase::Final,
            },
        }
    }

    fn wal_bytes(store: &Store) -> u64 {
        match fs::metadata(store.directory.join("transcript.sqlite-wal")) {
            Ok(metadata) => metadata.len(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => 0,
            Err(error) => panic!("WAL observation failed: {error}"),
        }
    }

    fn assert_body_and_copy(store: &Store, id: &str, expected: &str) {
        let mut body = Vec::new();
        for start in (0..expected.len()).step_by(super::super::PART_BYTES) {
            body.extend(
                store
                    .field(
                        id,
                        0,
                        start,
                        (start + super::super::PART_BYTES).min(expected.len()),
                    )
                    .unwrap(),
            );
        }
        assert_eq!(body, expected.as_bytes());
        let source = store.copy_parent_source(id).unwrap().unwrap();
        assert_eq!(source.bytes, expected.len() as u64);
        let mut copied = String::new();
        loop {
            let page = store.text_copy(&source, copied.len() as u64).unwrap();
            assert_eq!(page.offset, copied.len() as u64);
            copied.push_str(&page.text);
            if page.done {
                break;
            }
        }
        assert_eq!(copied, expected);
    }

    fn pin_reader(store: &Store) -> Connection {
        let reader = Connection::open(store.directory.join("transcript.sqlite")).unwrap();
        reader.execute_batch("BEGIN").unwrap();
        reader
            .query_row("SELECT count(*) FROM chunks", [], |r| r.get::<_, i64>(0))
            .unwrap();
        reader
    }

    fn is_busy(outcome: Outcome) -> bool {
        matches!(outcome, Outcome::Checkpoint { busy: 1, .. })
    }

    #[test]
    fn more_than_fifty_mebibytes_of_rewrites_keep_retained_wal_and_all_bytes_bounded() {
        let root = tempfile::tempdir().unwrap();
        let mut store = Store::at(root.path().to_owned()).unwrap();
        let mut other = Store::at(root.path().to_owned()).unwrap();
        for revision in 0..64 {
            let text = format!("{revision}:{}", "x".repeat(1024 * 1024));
            store
                .apply(Event::Item(prose("same", text.clone())))
                .unwrap();
            assert!(wal_bytes(&store) <= LIMIT, "rewrite {revision}");
            assert!(!store.directory.join("transcript.sqlite-journal").exists());
            assert_body_and_copy(&store, "same", &text);
        }
        assert_eq!(
            crate::transcript_cache::sweep(root.path()).unwrap().count,
            0
        );
        other
            .apply(Event::Item(prose("live", "still live".into())))
            .unwrap();
        assert_body_and_copy(&other, "live", "still live");
        assert_eq!(
            store
                .db
                .pragma_query_value(None, "journal_mode", |r| r.get::<_, String>(0))
                .unwrap(),
            "wal"
        );
        assert_eq!(
            store
                .db
                .pragma_query_value(None, "synchronous", |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            store
                .db
                .pragma_query_value(None, "cache_size", |r| r.get::<_, i64>(0))
                .unwrap(),
            -8192
        );
        assert_eq!(
            store
                .db
                .pragma_query_value(None, "journal_size_limit", |r| r.get::<_, u64>(0))
                .unwrap(),
            LIMIT
        );
    }

    #[test]
    fn one_large_commit_and_pending_text_flush_trim_without_losing_publication() {
        let root = tempfile::tempdir().unwrap();
        let mut store = Store::at(root.path().to_owned()).unwrap();
        let text = "large 🦀\n".repeat(1024 * 1024);
        store
            .apply(Event::Item(prose("large", text.clone())))
            .unwrap();
        assert!(wal_bytes(&store) <= LIMIT);
        assert_body_and_copy(&store, "large", &text);
        let reader = pin_reader(&store);
        store
            .apply(Event::Item(prose("pinned", "p".repeat(2 * 1024 * 1024))))
            .unwrap();
        assert!(wal_bytes(&store) > LIMIT);
        let delta = "y".repeat(64 * 1024);
        store
            .apply(Event::TextDelta {
                id: "large".into(),
                role: Role::Assistant,
                thinking: false,
                delta: delta.clone(),
            })
            .unwrap();
        assert!(store.pending_text.is_some());
        reader.execute_batch("ROLLBACK").unwrap();
        drop(reader);
        // No apply or direct maintenance call follows reader release. Only
        // the window's real pending-text append can trim this oversized WAL.
        store.window().unwrap();
        assert!(store.pending_text.is_none());
        assert!(wal_bytes(&store) <= LIMIT);
        assert_body_and_copy(&store, "large", &(text + &delta));
    }

    #[test]
    fn pinned_reader_keeps_its_snapshot_and_release_then_next_write_trims() {
        let root = tempfile::tempdir().unwrap();
        let mut store = Store::at(root.path().to_owned()).unwrap();
        store
            .apply(Event::Item(prose("same", "old".into())))
            .unwrap();
        let reader = pin_reader(&store);
        let text = "n".repeat(2 * 1024 * 1024);
        store
            .apply(Event::Item(prose("same", text.clone())))
            .unwrap();
        assert!(wal_bytes(&store) > LIMIT);
        assert!(is_busy(after_write(&store.db, &store.directory)));
        assert_eq!(
            reader
                .query_row(
                    "SELECT data FROM chunks WHERE id='same' AND field=0 AND position=0",
                    [],
                    |r| r.get::<_, Vec<u8>>(0)
                )
                .unwrap(),
            b"old"
        );
        assert_body_and_copy(&store, "same", &text);
        reader.execute_batch("ROLLBACK").unwrap();
        drop(reader);
        store
            .apply(Event::Item(prose("next", "next boundary".into())))
            .unwrap();
        assert!(wal_bytes(&store) <= LIMIT);
        assert_body_and_copy(&store, "same", &text);
    }

    unsafe extern "C" fn count_busy(
        context: *mut std::ffi::c_void,
        _: std::ffi::c_int,
    ) -> std::ffi::c_int {
        // SAFETY: the test retains this AtomicUsize until the callback is removed.
        unsafe { &*context.cast::<std::sync::atomic::AtomicUsize>() }
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        0
    }

    #[test]
    fn auxiliary_checkpoint_preserves_original_main_timeout_and_busy_handler() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let root = tempfile::tempdir().unwrap();
        let mut store = Store::at(root.path().to_owned()).unwrap();
        store.db.busy_timeout(Duration::from_millis(73)).unwrap();
        store
            .apply(Event::Item(prose("same", "old".into())))
            .unwrap();
        let reader = pin_reader(&store);
        store
            .apply(Event::Item(prose("same", "n".repeat(2 * 1024 * 1024))))
            .unwrap();
        assert!(is_busy(after_write(&store.db, &store.directory)));
        assert_eq!(
            store
                .db
                .pragma_query_value(None, "busy_timeout", |r| r.get::<_, i64>(0))
                .unwrap(),
            73
        );
        reader.execute_batch("ROLLBACK").unwrap();
        drop(reader);
        let writer = Connection::open(store.directory.join("transcript.sqlite")).unwrap();
        writer.execute_batch("BEGIN IMMEDIATE").unwrap();
        let calls = AtomicUsize::new(0);
        // SAFETY: the main connection is exclusively borrowed by this test;
        // calls remains alive until the handler is explicitly removed below.
        assert_eq!(
            unsafe {
                rusqlite::ffi::sqlite3_busy_handler(
                    store.db.handle(),
                    Some(count_busy),
                    (&calls as *const AtomicUsize).cast_mut().cast(),
                )
            },
            rusqlite::ffi::SQLITE_OK
        );
        let checkpoint = after_write(&store.db, &store.directory);
        let after_checkpoint = calls.load(Ordering::SeqCst);
        let blocked = store.db.execute("UPDATE items SET ordinal=ordinal", []);
        let after_write = calls.load(Ordering::SeqCst);
        // Remove before assertions, including when a negative control failed.
        assert_eq!(
            unsafe {
                rusqlite::ffi::sqlite3_busy_handler(store.db.handle(), None, std::ptr::null_mut())
            },
            rusqlite::ffi::SQLITE_OK
        );
        assert!(is_busy(checkpoint));
        assert_eq!(after_checkpoint, 0);
        assert!(
            matches!(blocked, Err(rusqlite::Error::SqliteFailure(error, _)) if error.code == rusqlite::ErrorCode::DatabaseBusy)
        );
        assert!(
            after_write > 0,
            "the original callback must still be installed"
        );
        writer.execute_batch("ROLLBACK").unwrap();
        drop(writer);
        store
            .apply(Event::Item(prose("next", "recovered".into())))
            .unwrap();
        assert!(wal_bytes(&store) <= LIMIT);
    }

    #[test]
    fn implicit_and_explicit_main_snapshots_are_not_released_by_maintenance() {
        let root = tempfile::tempdir().unwrap();
        let mut store = Store::at(root.path().to_owned()).unwrap();
        store
            .apply(Event::Item(prose("same", "old".into())))
            .unwrap();
        {
            let mut statement = store.db.prepare("SELECT data FROM chunks").unwrap();
            let mut rows = statement.query([]).unwrap();
            assert!(rows.next().unwrap().is_some());
            assert!(store.db.is_autocommit());
            assert_eq!(
                after_write(&store.db, &store.directory),
                Outcome::DeferredTransaction
            );
            assert!(matches!(
                store.db.transaction_state::<&str>(None).unwrap(),
                TransactionState::Read
            ));
        }
        store
            .db
            .execute_batch("BEGIN; SELECT data FROM chunks")
            .unwrap();
        assert_eq!(
            after_write(&store.db, &store.directory),
            Outcome::DeferredTransaction
        );
        assert!(!store.db.is_autocommit());
        store.db.execute_batch("ROLLBACK").unwrap();
    }

    #[test]
    fn rollback_and_secondary_index_error_keep_original_data_and_error_semantics() {
        let root = tempfile::tempdir().unwrap();
        let mut store = Store::at(root.path().to_owned()).unwrap();
        store
            .apply(Event::Item(prose("same", "old".into())))
            .unwrap();
        let original_copy = store.copy_parent_source("same").unwrap().unwrap();
        store.db.execute_batch("CREATE TRIGGER refuse_chunk BEFORE INSERT ON chunks WHEN NEW.position>0 BEGIN SELECT RAISE(ABORT,'chunk write refused'); END;").unwrap();
        let error = store
            .apply(Event::Item(prose("same", "x".repeat(2 * 1024 * 1024))))
            .unwrap_err();
        assert!(error.contains("chunk write refused"));
        assert!(store.db.is_autocommit());
        assert_body_and_copy(&store, "same", "old");
        assert_eq!(
            store.copy_parent_source("same").unwrap().unwrap(),
            original_copy
        );
        store.db.execute_batch("DROP TRIGGER refuse_chunk").unwrap();
        let reader = pin_reader(&store);
        store.db.execute_batch("DROP TABLE file_edits").unwrap();
        let replacement = "new".repeat(512 * 1024);
        let error = store
            .apply(Event::Item(prose("same", replacement.clone())))
            .unwrap_err();
        assert!(error.contains("file_edits"));
        assert!(is_busy(after_write(&store.db, &store.directory)));
        assert_body_and_copy(&store, "same", &replacement);
        assert_ne!(
            store.copy_parent_source("same").unwrap().unwrap().revision,
            original_copy.revision
        );
        reader.execute_batch("ROLLBACK").unwrap();
        drop(reader);
        let final_text = "final".repeat(512 * 1024);
        let error = store
            .apply(Event::Item(prose("same", final_text.clone())))
            .unwrap_err();
        assert!(error.contains("file_edits"));
        // No direct helper call: the body committed but the original index
        // error must still pass through post-error maintenance.
        assert!(wal_bytes(&store) <= LIMIT);
        assert_body_and_copy(&store, "same", &final_text);
    }

    #[test]
    fn failed_pending_append_keeps_its_bytes_and_retry_commits_them_once() {
        let root = tempfile::tempdir().unwrap();
        let mut store = Store::at(root.path().to_owned()).unwrap();
        store
            .apply(Event::Item(prose("same", "old".into())))
            .unwrap();
        let delta = "retry 🦀".repeat(1024);
        store
            .apply(Event::TextDelta {
                id: "same".into(),
                role: Role::Assistant,
                thinking: false,
                delta: delta.clone(),
            })
            .unwrap();
        store.db.execute_batch("CREATE TRIGGER refuse_append BEFORE UPDATE ON items BEGIN SELECT RAISE(ABORT,'append metadata refused'); END;").unwrap();
        let error = match store.window() {
            Ok(_) => panic!("the actual append must fail"),
            Err(error) => error,
        };
        assert!(error.contains("append metadata refused"));
        assert_eq!(store.pending_text.as_ref().unwrap().3, delta);
        assert_body_and_copy(&store, "same", "old");
        store
            .db
            .execute_batch("DROP TRIGGER refuse_append")
            .unwrap();
        store.window().unwrap();
        assert!(store.pending_text.is_none());
        assert_body_and_copy(&store, "same", &("old".to_owned() + &delta));
        assert!(wal_bytes(&store) <= LIMIT);
    }

    #[test]
    fn missing_nonregular_and_redirected_paths_never_become_success_or_touch_other_cache() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let mut store = Store::at(root.path().to_owned()).unwrap();
        let other = Store::at(root.path().to_owned()).unwrap();
        store
            .apply(Event::Item(prose("same", "old".into())))
            .unwrap();
        assert_eq!(
            after_write(&store.db, &root.path().join("missing")),
            Outcome::Unavailable
        );
        let database = store.directory.join("transcript.sqlite");
        let held = store.directory.join("held-database.sqlite");
        fs::rename(&database, &held).unwrap();
        symlink(other.directory.join("transcript.sqlite"), &database).unwrap();
        let before = fs::read(other.directory.join("transcript.sqlite")).unwrap();
        let outcome = after_write(&store.db, &store.directory);
        let after = fs::read(other.directory.join("transcript.sqlite")).unwrap();
        fs::remove_file(&database).unwrap();
        fs::rename(&held, &database).unwrap();
        assert_eq!(outcome, Outcome::Unavailable);
        assert_eq!(before, after);
        assert_body_and_copy(&store, "same", "old");
        let wal = store.directory.join("transcript.sqlite-wal");
        let held_wal = store.directory.join("held-wal");
        let foreign = root.path().join("foreign-evidence");
        fs::write(&foreign, b"must not be checkpointed or truncated").unwrap();
        fs::rename(&wal, &held_wal).unwrap();
        symlink(&foreign, &wal).unwrap();
        let outcome = after_write(&store.db, &store.directory);
        let preserved = fs::read(&foreign).unwrap();
        fs::remove_file(&wal).unwrap();
        fs::rename(&held_wal, &wal).unwrap();
        assert_eq!(outcome, Outcome::Unavailable);
        assert_eq!(preserved, b"must not be checkpointed or truncated");
        let fake = root.path().join("not-a-database");
        fs::write(&fake, b"not sqlite").unwrap();
        assert_eq!(after_write(&store.db, &fake), Outcome::Unavailable);
    }
}
