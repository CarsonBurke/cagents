use serde_json::Value;
use std::{
    fs, io,
    os::unix::fs::MetadataExt,
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

// Shared with the REST fallback in scripts/gh-pr-status.
const SWEEP_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
const RETENTION: Duration = Duration::from_secs(30 * SWEEP_INTERVAL.as_secs());
const SWEEP_LOCK: &str = ".retention.lock";

/// A status read reclaims old records opportunistically without waiting for
/// maintenance or interrupting any deadline or in-flight publication.
pub(crate) fn prune_cache(root: &Path, requested: &str) {
    let _ = prune_at(root, requested, SystemTime::now());
}

fn prune_at(root: &Path, requested: &str, at: SystemTime) -> io::Result<()> {
    let marker = root.join(SWEEP_LOCK);
    if fs::symlink_metadata(&marker).is_ok_and(|metadata| swept_recently(&metadata, at)) {
        return Ok(());
    }
    let sweep = crate::instance::owned_lock(&marker, libc::LOCK_EX | libc::LOCK_NB)?;
    let metadata = sweep.metadata()?;
    if metadata.nlink() != 1 || swept_recently(&metadata, at) {
        return Ok(());
    }
    io::Write::write_all(&mut &*sweep, b"1")?;

    let now = at.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    let uid = fs::symlink_metadata(root)?.uid();
    for entry in fs::read_dir(root)?.flatten() {
        let file_name = entry.file_name();
        let Some((key, kind)) = file_name.to_str().and_then(record_name) else {
            continue;
        };
        if key == requested || !entry.metadata().is_ok_and(|found| expired(&found, uid, at)) {
            continue;
        }
        let Ok(_flight) = crate::instance::owned_lock(
            &root.join(format!("{key}.lock")),
            libc::LOCK_EX | libc::LOCK_NB,
        ) else {
            continue;
        };
        let path = entry.path();
        // Publication may have replaced the entry between listing and locking.
        if fs::symlink_metadata(&path).is_ok_and(|found| expired(&found, uid, at))
            && super::load::<Value>(&path)
                .ok()
                .flatten()
                .and_then(|record| deadline(kind, &record))
                .is_some_and(|due| due <= now)
        {
            let _ = fs::remove_file(path);
        }
    }
    Ok(())
}

fn swept_recently(metadata: &fs::Metadata, at: SystemTime) -> bool {
    metadata.is_file()
        && metadata.len() > 0
        && age(metadata, at).is_some_and(|age| age < SWEEP_INTERVAL)
}

fn expired(metadata: &fs::Metadata, uid: u32, at: SystemTime) -> bool {
    metadata.is_file()
        && metadata.uid() == uid
        && metadata.nlink() == 1
        && age(metadata, at).is_some_and(|age| age >= RETENTION)
}

fn age(metadata: &fs::Metadata, at: SystemTime) -> Option<Duration> {
    at.duration_since(metadata.modified().ok()?).ok()
}

fn record_name(name: &str) -> Option<(&str, &str)> {
    let (key, kind) = name.split_once('.')?;
    (key.len() == 64
        && key
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        && matches!(kind, "json" | "rest-retry" | "cooldown"))
    .then_some((key, kind))
}

fn deadline(kind: &str, record: &Value) -> Option<u64> {
    match kind {
        // PR status and review-request census records share this extension.
        "json" => match (record.get("next_attempt"), record.get("due")) {
            (Some(pr), Some(review)) => Some(pr.as_u64()?.max(review.as_u64()?)),
            (Some(due), None) | (None, Some(due)) => due.as_u64(),
            (None, None) => None,
        },
        "rest-retry" => record.get("retry_at")?.as_u64(),
        // Older clients used a bare timestamp. Check all resources in v2.
        "cooldown" => record.as_u64().or_else(|| {
            record
                .get("primary")?
                .as_object()?
                .values()
                .try_fold(record.get("secondary")?.as_u64()?, |due, resource| {
                    resource.as_u64().map(|resource| due.max(resource))
                })
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::os::unix::fs::symlink;

    #[test]
    fn old_idle_records_expire_without_losing_locks_or_live_deadlines() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        let at = SystemTime::now();
        let old = at - RETENTION - Duration::from_secs(1);
        let future = at.duration_since(UNIX_EPOCH).unwrap().as_secs() + 60;
        let record_path =
            |digit: &str, suffix: &str| root.join(format!("{}.{suffix}", digit.repeat(64)));
        let records = [
            ("1", "json", json!({"next_attempt": 0}), false),
            ("2", "json", json!({"due": 0}), false),
            ("3", "rest-retry", json!({"retry_at": 0}), false),
            (
                "4",
                "cooldown",
                json!({"secondary": 0, "primary": {"core": 1}}),
                false,
            ),
            ("5", "cooldown", json!(1), false),
            ("6", "json", json!({"next_attempt": future}), true),
            ("7", "json", json!({"due": future}), true),
            ("8", "rest-retry", json!({"retry_at": future}), true),
            (
                "9",
                "cooldown",
                json!({"secondary": 1, "primary": {"graphql": future}}),
                true,
            ),
            (
                "a",
                "cooldown",
                json!({"secondary": future, "primary": {}}),
                true,
            ),
            ("b", "cooldown", json!(future), true),
            ("c", "json", json!({"next_attempt": 0}), true),
            ("d", "json", json!({"next_attempt": 0}), true),
            ("e", "json", json!({"unrecognized": 1}), true),
        ];
        for (digit, suffix, data, _) in &records {
            let path = record_path(digit, suffix);
            super::super::store(&path, data).unwrap();
            fs::File::open(path).unwrap().set_modified(old).unwrap();
        }
        let fresh = record_path("0", "json");
        super::super::store(&fresh, &json!({"due": 0})).unwrap();
        let link = record_path("f", "cooldown");
        symlink(record_path("b", "cooldown"), &link).unwrap();
        let busy = crate::instance::owned_lock(&record_path("c", "lock"), libc::LOCK_EX).unwrap();
        let unlocked = record_path("1", "lock");
        let lock = crate::instance::owned_lock(&unlocked, libc::LOCK_EX).unwrap();
        let inode = lock.metadata().unwrap().ino();
        drop(lock);

        prune_at(root, &"d".repeat(64), at).unwrap();
        for (digit, suffix, _, keep) in records {
            assert_eq!(
                record_path(digit, suffix).exists(),
                keep,
                "{digit}.{suffix}"
            );
        }
        assert!(fresh.exists());
        assert!(link.is_symlink());
        assert_eq!(fs::metadata(unlocked).unwrap().ino(), inode);
        drop(busy);
        fs::File::open(root.join(SWEEP_LOCK))
            .unwrap()
            .set_modified(at)
            .unwrap();
        let now = at + SWEEP_INTERVAL - Duration::from_secs(1);
        prune_at(root, "", now).unwrap();
        assert!(record_path("c", "json").exists());
        prune_at(root, "", at + SWEEP_INTERVAL).unwrap();
        assert!(!record_path("c", "json").exists());
    }
}
