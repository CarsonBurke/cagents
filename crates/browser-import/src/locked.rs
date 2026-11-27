//! Reads a database that its browser holds under an exclusive SQLite lock.
//!
//! Firefox and its forks open `cookies.sqlite` in exclusive locking mode
//! while they run, so no other SQLite connection can read it. The files stay
//! readable, though. This copies the database and its write-ahead log into
//! memory, replays the log's committed transactions as SQLite would, and
//! leaves nothing on disk. A copy that changed while it was read is
//! discarded and read again.

use crate::{Error, cancelled};
use std::{
    fs::File,
    io::Read,
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
    time::{Duration, SystemTime},
};
use zeroize::Zeroizing;

const ATTEMPTS: usize = 5;
const WAL_HEADER: usize = 32;
const FRAME_HEADER: usize = 24;
/// A rollback journal begins with this while a transaction may be half
/// written into the database; SQLite would roll it back before reading.
const JOURNAL_MAGIC: [u8; 8] = [0xd9, 0xd5, 0x05, 0xf9, 0x20, 0xa1, 0x63, 0xd7];

/// The database's bytes, with committed log transactions applied, ready to
/// deserialize. At most `limit` bytes are read from either file.
pub(crate) fn image(
    path: &Path,
    limit: u64,
    cancel: &AtomicBool,
) -> Result<Zeroizing<Vec<u8>>, Error> {
    let wal_path = sibling(path, "-wal");
    let journal_path = sibling(path, "-journal");
    for attempt in 0..ATTEMPTS {
        cancelled(cancel)?;
        if attempt > 0 {
            std::thread::sleep(Duration::from_millis(50));
        }
        let before = Files::observe(path, &wal_path, &journal_path)?;
        if before.journal_hot {
            continue;
        }
        // A file that shrank while it was read changed; read both again.
        let (image, wal) = match (read(path, limit), read(&wal_path, limit)) {
            (Err(Error::Busy), _) | (_, Err(Error::Busy)) => continue,
            (image, wal) => (image?, wal?),
        };
        let Some(mut image) = image else {
            return Err(Error::Unreadable);
        };
        // The log is read after the database: a checkpoint copying log pages
        // into the database meanwhile is harmless, since replaying the log
        // rewrites those pages. Only a restarted log or a changed file
        // means the two copies may not belong together.
        if Files::observe(path, &wal_path, &journal_path)? != before
            || wal.as_ref().and_then(|wal| wal.get(..WAL_HEADER)) != before.wal_header.as_deref()
        {
            continue;
        }
        if let Some(wal) = wal {
            replay(&mut image, &wal, limit)?;
        }
        without_log(&mut image)?;
        return Ok(image);
    }
    Err(Error::Busy)
}

/// Bytes 18 and 19 are 2 for a WAL database. The copy has no log, so mark
/// it as a rollback-journal database for SQLite to open it from memory.
fn without_log(image: &mut [u8]) -> Result<(), Error> {
    let versions = image.get_mut(18..20).ok_or(Error::Damaged)?;
    for byte in versions {
        if *byte == 2 {
            *byte = 1;
        }
    }
    Ok(())
}

fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    name.into()
}

#[derive(PartialEq, Eq)]
struct Files {
    /// Length, modification time, and the header's change counter, which a
    /// rollback-journal commit always increments even within one clock tick.
    database: (u64, Option<SystemTime>, Option<Vec<u8>>),
    wal_header: Option<Vec<u8>>,
    journal_hot: bool,
}

impl Files {
    fn observe(database: &Path, wal: &Path, journal: &Path) -> Result<Self, Error> {
        let metadata = std::fs::metadata(database).map_err(|_| Error::Unreadable)?;
        Ok(Self {
            database: (
                metadata.len(),
                metadata.modified().ok(),
                prefix(database, 28).map(|header| header[24..].to_vec()),
            ),
            wal_header: prefix(wal, WAL_HEADER),
            journal_hot: prefix(journal, JOURNAL_MAGIC.len())
                .is_some_and(|start| start == JOURNAL_MAGIC),
        })
    }
}

/// The first `length` bytes of a file, or `None` when it is missing or shorter.
fn prefix(path: &Path, length: usize) -> Option<Vec<u8>> {
    let mut bytes = vec![0; length];
    File::open(path).ok()?.read_exact(&mut bytes).ok()?;
    Some(bytes)
}

/// A whole file, or `None` when it does not exist. The buffer is sized once
/// from the file's length, so no partial copy is left behind by growth.
fn read(path: &Path, limit: u64) -> Result<Option<Zeroizing<Vec<u8>>>, Error> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(Error::Unreadable),
    };
    let length = file.metadata().map_err(|_| Error::Unreadable)?.len();
    if length > limit {
        return Err(Error::TooLarge);
    }
    let mut bytes = Zeroizing::new(vec![0; length as usize]);
    // A file that shrank fails here, reported as busy so the caller reads
    // again; one that grew changes its observed length.
    file.read_exact(&mut bytes).map_err(|_| Error::Busy)?;
    Ok(Some(bytes))
}

fn be32(bytes: &[u8]) -> u32 {
    u32::from_be_bytes(bytes[..4].try_into().unwrap())
}

/// SQLite's cumulative log checksum over 32-bit word pairs.
fn checksum(big_endian: bool, bytes: &[u8], (mut s0, mut s1): (u32, u32)) -> (u32, u32) {
    for pair in bytes.chunks_exact(8) {
        let word = |bytes: &[u8]| {
            let bytes = bytes.try_into().unwrap();
            if big_endian {
                u32::from_be_bytes(bytes)
            } else {
                u32::from_le_bytes(bytes)
            }
        };
        s0 = s0.wrapping_add(word(&pair[..4])).wrapping_add(s1);
        s1 = s1.wrapping_add(word(&pair[4..])).wrapping_add(s0);
    }
    (s0, s1)
}

/// Applies the log's committed frames to `image`, following the format in
/// <https://www.sqlite.org/fileformat2.html#walformat>. A log SQLite itself
/// would ignore, such as one with a bad header, changes nothing; frames
/// after the last valid commit are an unfinished transaction and are dropped.
fn replay(image: &mut Zeroizing<Vec<u8>>, wal: &[u8], limit: u64) -> Result<(), Error> {
    let Some(header) = wal.get(..WAL_HEADER) else {
        return Ok(());
    };
    let big_endian = match be32(header) {
        0x377f_0682 => false,
        0x377f_0683 => true,
        _ => return Ok(()),
    };
    let page_size = be32(&header[8..]) as usize;
    if be32(&header[4..]) != 3_007_000
        || !(512..=65536).contains(&page_size)
        || !page_size.is_power_of_two()
        || checksum(big_endian, &header[..24], (0, 0)) != (be32(&header[24..]), be32(&header[28..]))
    {
        return Ok(());
    }
    if image.len() >= 100 {
        let stored = match u16::from_be_bytes([image[16], image[17]]) {
            1 => 65536,
            size => usize::from(size),
        };
        if stored != page_size {
            return Err(Error::Damaged);
        }
    }
    // First find the committed frames and the final size, so the image is
    // allocated once: growing it in place would leave unerased copies of
    // cookie data behind in freed memory.
    let salt = &header[16..24];
    let mut sums = (be32(&header[24..]), be32(&header[28..]));
    let mut frames = Vec::new();
    let mut committed = (0, None);
    for frame in wal[WAL_HEADER..].chunks_exact(FRAME_HEADER + page_size) {
        let (frame_header, page) = frame.split_at(FRAME_HEADER);
        if &frame_header[8..16] != salt {
            break;
        }
        sums = checksum(big_endian, &frame_header[..8], sums);
        sums = checksum(big_endian, page, sums);
        if sums != (be32(&frame_header[16..]), be32(&frame_header[20..])) {
            break;
        }
        let page_number = be32(frame_header) as usize;
        if page_number == 0 {
            break;
        }
        frames.push((page_number, page));
        let pages = be32(&frame_header[4..]) as usize;
        if pages != 0 {
            let end = pages
                .checked_mul(page_size)
                .filter(|&end| end as u64 <= limit)
                .ok_or(Error::TooLarge)?;
            committed = (frames.len(), Some(end));
        }
    }
    let (count, Some(end)) = committed else {
        return Ok(());
    };
    let mut replayed = Zeroizing::new(Vec::with_capacity(end));
    replayed.extend_from_slice(&image[..image.len().min(end)]);
    replayed.resize(end, 0);
    for &(page_number, page) in &frames[..count] {
        // A page beyond the committed size was truncated by a later commit.
        if let Some(start) = (page_number - 1).checked_mul(page_size)
            && start + page_size <= end
        {
            replayed[start..start + page_size].copy_from_slice(page);
        }
    }
    *image = replayed;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn rows(image: &[u8]) -> Vec<String> {
        let mut image = image.to_vec();
        without_log(&mut image).unwrap();
        let mut connection = Connection::open_in_memory().unwrap();
        connection
            .deserialize_read_exact(rusqlite::MAIN_DB, &image[..], image.len(), true)
            .unwrap();
        connection
            .prepare("SELECT host FROM moz_cookies ORDER BY host")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    /// The way Firefox holds `cookies.sqlite` while it runs: WAL mode under
    /// an exclusive lock, with recent commits only in the log.
    #[test]
    fn replays_committed_log_frames_from_an_exclusively_locked_database() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("cookies.sqlite");
        let source = Connection::open(&path).unwrap();
        source
            .execute_batch(
                "PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;
                 CREATE TABLE moz_cookies(host TEXT);
                 INSERT INTO moz_cookies VALUES('checkpointed.test');
                 PRAGMA wal_checkpoint(TRUNCATE);
                 PRAGMA locking_mode=EXCLUSIVE;
                 INSERT INTO moz_cookies VALUES('logged.test');
                 INSERT INTO moz_cookies VALUES('second.test');
                 BEGIN; INSERT INTO moz_cookies VALUES('uncommitted.test');",
            )
            .unwrap();
        // SQLite itself can't read it now.
        let reader =
            Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        reader.busy_timeout(Duration::ZERO).unwrap();
        assert!(
            reader
                .query_row("SELECT count(*) FROM moz_cookies", [], |row| row
                    .get::<_, i64>(0))
                .is_err()
        );
        let image = image(&path, 1 << 20, &AtomicBool::new(false)).unwrap();
        assert_eq!(
            rows(&image),
            ["checkpointed.test", "logged.test", "second.test"]
        );
        source.execute_batch("COMMIT").unwrap();
        let image = super::image(&path, 1 << 20, &AtomicBool::new(false)).unwrap();
        assert_eq!(rows(&image).len(), 4);
    }

    #[test]
    fn a_torn_or_foreign_log_tail_is_ignored() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("cookies.sqlite");
        let source = Connection::open(&path).unwrap();
        source
            .execute_batch(
                "PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; PRAGMA locking_mode=EXCLUSIVE;
                 CREATE TABLE moz_cookies(host TEXT);
                 INSERT INTO moz_cookies VALUES('kept.test');",
            )
            .unwrap();
        let database = std::fs::read(&path).unwrap();
        let mut wal = std::fs::read(sibling(&path, "-wal")).unwrap();
        let mut whole = Zeroizing::new(database.clone());
        replay(&mut whole, &wal, 1 << 20).unwrap();
        assert_eq!(rows(&whole), ["kept.test"]);
        // Corrupt the last frame's page: its commit no longer verifies, so
        // the transaction that inserted the row is dropped.
        let last = wal.len() - 1;
        wal[last] ^= 0xff;
        let mut torn = Zeroizing::new(database.clone());
        replay(&mut torn, &wal, 1 << 20).unwrap();
        assert_ne!(*torn, *whole);
        // A log with a bad header is ignored entirely, as SQLite does.
        wal[0] = 0;
        let mut ignored = Zeroizing::new(database.clone());
        replay(&mut ignored, &wal, 1 << 20).unwrap();
        assert_eq!(*ignored, database);
        // The log can't grow the database past the import limit.
        let wal = std::fs::read(sibling(&path, "-wal")).unwrap();
        assert_eq!(
            replay(&mut Zeroizing::new(database), &wal, 1024).err(),
            Some(Error::TooLarge)
        );
    }

    #[test]
    fn a_half_written_rollback_transaction_is_not_read() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("Cookies");
        std::fs::write(&path, [0; 512]).unwrap();
        let mut journal = JOURNAL_MAGIC.to_vec();
        journal.resize(512, 0);
        std::fs::write(sibling(&path, "-journal"), journal).unwrap();
        assert_eq!(
            image(&path, 1 << 20, &AtomicBool::new(false)).err(),
            Some(Error::Busy)
        );
        assert_eq!(
            image(&path, 1 << 20, &AtomicBool::new(true)).err(),
            Some(Error::Cancelled)
        );
    }
}
