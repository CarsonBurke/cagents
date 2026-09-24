//! Codex turn state from the rollout files a live Codex process holds open.
//!
//! A standalone Codex CLI answers no shared app-server, but it appends each
//! turn's boundaries to its thread's rollout as they happen, and keeps that
//! file open for writing for as long as the thread is loaded. A thread runs
//! one turn at a time, so the last boundary in its rollout is its state. A
//! turn cut short by a crash has no end recorded; a process that resumes the
//! thread did not start it, so a turn begun before the process was is over.
//!
//! Each file is read backwards once, to its last boundary, and afterwards only
//! forwards from where the previous observation stopped. Whether a turn waits
//! on the user is not recorded, so a turn waiting for approval reads as work.
use super::ActivityState;
use serde::Deserialize;
use std::{
    borrow::Cow,
    collections::BTreeMap,
    ffi::{OsStr, OsString},
    fs::{File, OpenOptions},
    io,
    os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt},
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

/// Bytes read per system call, in either direction.
const CHUNK: usize = 64 * 1024;
/// Boundary records are small, a few kilobytes at most; a longer line is
/// content, never one of them.
const MAX_LINE: usize = 256 * 1024;
/// How far back a file seen for the first time is searched for a boundary.
const MAX_SCAN: u64 = 32 * 1024 * 1024;
/// How far one observation reads forward; the rest waits for the next.
const MAX_READ: u64 = 8 * 1024 * 1024;
/// Descriptors of one process inspected at most.
const MAX_FDS: usize = 4096;
/// Files followed at once: several per process, as each loaded sub-agent
/// thread has its own.
const MAX_CURSORS: usize = 4096;
/// A file no process has been seen holding for this long is let go when
/// another is first seen.
const CURSOR_IDLE: Duration = Duration::from_secs(10 * 60);
/// How far a turn may be stamped before its process started and still be
/// its own: the stamp is in whole seconds, the process's start in ticks and
/// taken back from its age, which a wall clock stepped since skews.
const START_SLACK: u64 = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Turn {
    /// Begun at the unix second its record states, where it states one.
    Running(Option<u64>),
    Ended,
}

/// Where the next forward read starts, and what has been read so far.
#[derive(Debug)]
struct Cursor {
    offset: u64,
    /// The last boundary, or `None` while none has been found within bounds.
    turn: Option<Turn>,
    /// The line being written when the last read reached the end of the file.
    partial: Vec<u8>,
    /// That line outgrew [`MAX_LINE`] and is skipped to its end.
    overlong: bool,
}

/// One file, as long as its name and inode are the same.
type FileKey = (u64, u64, OsString);

/// Each file followed, with when a process was last seen holding it.
type Cursors = BTreeMap<FileKey, (Instant, Arc<Mutex<Cursor>>)>;

#[derive(Default)]
pub(super) struct Rollouts {
    cursors: Mutex<Cursors>,
}

impl Rollouts {
    /// The state of the Codex process `pid`, which started at the unix second
    /// `started` where that is known, from the rollouts it writes: working
    /// while any of its threads is mid-turn. `None` when it writes none, as a
    /// client of a shared server does, or when they cannot be listed.
    pub(super) fn state(&self, pid: i32, started: Option<u64>) -> Option<ActivityState> {
        let process = Path::new("/proc").join(pid.to_string());
        let mut state = None;
        for entry in std::fs::read_dir(process.join("fd")).ok()?.take(MAX_FDS) {
            let Ok(entry) = entry else { continue };
            let Ok(target) = std::fs::read_link(entry.path()) else {
                continue;
            };
            let Some(name) = target.file_name().filter(|name| is_rollout(name)) else {
                continue;
            };
            // Something that only reads a rollout - a picker listing past
            // sessions - says nothing about that thread's turns.
            if !opened_for_writing(&process, &entry.file_name()) {
                continue;
            }
            // Only a regular file is opened, and never so that the open can
            // wait or take a terminal, as a pipe or device of that name could.
            if !std::fs::metadata(entry.path()).is_ok_and(|metadata| metadata.is_file()) {
                continue;
            }
            // The descriptor, not the path: the exact file this process
            // writes, wherever its account keeps it.
            let Ok(file) = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY | libc::O_CLOEXEC)
                .open(entry.path())
            else {
                continue;
            };
            let turn = self.turn(&file, name.to_owned());
            state = Some(merge(state, activity(turn, started)));
        }
        state
    }

    fn turn(&self, file: &File, name: OsString) -> Option<Turn> {
        let metadata = file.metadata().ok().filter(|metadata| metadata.is_file())?;
        let key = (metadata.dev(), metadata.ino(), name);
        let cursor = {
            let mut cursors = self.cursors.lock().unwrap_or_else(|p| p.into_inner());
            if !cursors.contains_key(&key) {
                // Files no process holds any longer, unless one is being
                // read now.
                cursors.retain(|_, (seen, cursor)| {
                    seen.elapsed() < CURSOR_IDLE || Arc::strong_count(cursor) > 1
                });
            }
            if !cursors.contains_key(&key) && cursors.len() >= MAX_CURSORS {
                // The file observed longest ago, unless it is being read now.
                let stale = cursors
                    .iter()
                    .filter(|(_, (_, cursor))| Arc::strong_count(cursor) == 1)
                    .min_by_key(|(_, (seen, _))| *seen)
                    .map(|(key, _)| key.clone())?;
                cursors.remove(&stale);
            }
            let (seen, cursor) = cursors.entry(key).or_insert_with(|| {
                (
                    Instant::now(),
                    Arc::new(Mutex::new(Cursor {
                        offset: u64::MAX,
                        turn: None,
                        partial: Vec::new(),
                        overlong: false,
                    })),
                )
            });
            *seen = Instant::now();
            Arc::clone(cursor)
        };
        // Only this file's lock spans its reads. The size is read under it, so
        // a read that waited for another's is never behind where that one
        // left the cursor.
        let mut cursor = cursor.lock().unwrap_or_else(|p| p.into_inner());
        let size = file.metadata().ok()?.len();
        // A file rewritten shorter is not the one read before.
        if cursor.offset > size {
            *cursor = sight(file, size).ok()?;
        }
        advance(&mut cursor, file, size).ok()?;
        cursor.turn
    }
}

fn is_rollout(name: &OsStr) -> bool {
    let name = name.as_encoded_bytes();
    name.starts_with(b"rollout-") && name.ends_with(b".jsonl")
}

/// Whether descriptor `fd` of `process` was opened for writing, as its
/// `fdinfo` states in octal.
fn opened_for_writing(process: &Path, fd: &OsStr) -> bool {
    let Ok(info) = std::fs::read_to_string(process.join("fdinfo").join(fd)) else {
        return false;
    };
    info.lines()
        .find_map(|line| line.strip_prefix("flags:"))
        .and_then(|flags| i32::from_str_radix(flags.trim(), 8).ok())
        .is_some_and(|flags| flags & libc::O_ACCMODE != libc::O_RDONLY)
}

/// What a thread's last boundary says of a process that started at the unix
/// second `started`. A turn begun well before that was left unfinished by
/// another process - a crash the thread was since resumed from, or history
/// a fork copied - and this one is not running it.
fn activity(turn: Option<Turn>, started: Option<u64>) -> ActivityState {
    match turn {
        Some(Turn::Running(Some(at)))
            if started.is_some_and(|started| at.saturating_add(START_SLACK) < started) =>
        {
            ActivityState::Idle
        }
        Some(Turn::Running(_)) => ActivityState::Working,
        Some(Turn::Ended) => ActivityState::Idle,
        None => ActivityState::Unknown,
    }
}

/// Work outranks what is unknown, which outranks rest: a thread known to be
/// mid-turn is work however its siblings read.
fn merge(state: Option<ActivityState>, turn: ActivityState) -> ActivityState {
    let rank = |state| match state {
        ActivityState::Working => 3,
        ActivityState::Unknown => 2,
        ActivityState::Waiting => 1,
        ActivityState::Idle => 0,
    };
    match state {
        Some(state) if rank(state) >= rank(turn) => state,
        _ => turn,
    }
}

/// The turn boundary `line` records, if it is one.
fn boundary(line: &[u8]) -> Option<Turn> {
    // The stamps are read as whatever they are, so one of an unexpected
    // form costs only the stamp, never the boundary.
    #[derive(Deserialize)]
    struct Kind {
        #[serde(rename = "type")]
        kind: String,
        #[serde(default)]
        started_at: serde_json::Value,
    }
    #[derive(Deserialize)]
    struct Record {
        #[serde(rename = "type")]
        kind: String,
        #[serde(default)]
        timestamp: serde_json::Value,
        payload: Kind,
    }
    const MARKERS: [&[u8]; 3] = [
        b"\"task_started\"",
        b"\"task_complete\"",
        b"\"turn_aborted\"",
    ];
    // Nearly every line is content; only a line naming a boundary is parsed,
    // and only its record and payload types decide.
    if !MARKERS
        .iter()
        .any(|marker| line.windows(marker.len()).any(|window| window == *marker))
    {
        return None;
    }
    let record: Record = serde_json::from_slice(line).ok()?;
    if record.kind != "event_msg" {
        return None;
    }
    match record.payload.kind.as_str() {
        // Codex has stated when a turn began only since 2026; before, the
        // record's own time is when it was written, at the turn's start.
        "task_started" => Some(Turn::Running(
            record
                .payload
                .started_at
                .as_f64()
                .filter(|at| at.is_finite() && *at >= 0.0)
                .map(|at| at as u64)
                .or_else(|| record.timestamp.as_str().and_then(unix_seconds)),
        )),
        "task_complete" | "turn_aborted" => Some(Turn::Ended),
        _ => None,
    }
}

/// The unix second a UTC RFC 3339 time such as `2026-03-01T12:34:56.789Z`
/// names, as Codex stamps its records.
fn unix_seconds(time: &str) -> Option<u64> {
    let bytes = time.as_bytes();
    if bytes.len() < 20
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
        || !time.ends_with('Z')
    {
        return None;
    }
    let field = |range: std::ops::Range<usize>| time.get(range)?.parse::<i64>().ok();
    let (year, month, day) = (field(0..4)?, field(5..7)?, field(8..10)?);
    let (hour, minute, second) = (field(11..13)?, field(14..16)?, field(17..19)?);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59 {
        return None;
    }
    // Days since the epoch of a proleptic Gregorian date, counting years
    // from March so that a leap day ends its year.
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    u64::try_from(days * 86_400 + hour * 3_600 + minute * 60 + second).ok()
}

/// A cursor for a file seen for the first time: its last boundary, searched
/// for backwards from the end of its last complete line, where forward reads
/// then begin.
fn sight(file: &File, size: u64) -> io::Result<Cursor> {
    let mut position = size;
    // The start of the line whose end has been read, up to that end.
    let mut pending: Vec<u8> = Vec::new();
    let mut overlong = false;
    // Just past the file's last newline: what follows is still being written.
    let mut complete: Option<u64> = None;
    let mut scanned = 0;
    let mut chunk = vec![0; CHUNK];
    let turn = loop {
        if position == 0 {
            // `pending` is the file's first line. A file without a boundary
            // has had no turn yet.
            if complete.is_some()
                && !overlong
                && let Some(turn) = boundary(&pending)
            {
                break Some(turn);
            }
            break Some(Turn::Ended);
        }
        if scanned >= MAX_SCAN {
            break None;
        }
        let start = position.saturating_sub(CHUNK as u64);
        let length = (position - start) as usize;
        file.read_exact_at(&mut chunk[..length], start)?;
        scanned += length as u64;
        let bytes = &chunk[..length];
        let mut end = length;
        let mut found = None;
        for newline in memrchr_iter(bytes) {
            if complete.is_none() {
                complete = Some(start + newline as u64 + 1);
            } else if !overlong && end - newline - 1 + pending.len() <= MAX_LINE {
                let line = if pending.is_empty() {
                    Cow::Borrowed(&bytes[newline + 1..end])
                } else {
                    Cow::Owned([&bytes[newline + 1..end], &pending[..]].concat())
                };
                if let Some(turn) = boundary(&line) {
                    found = Some(turn);
                    break;
                }
            }
            pending.clear();
            overlong = false;
            end = newline;
        }
        if found.is_some() {
            break found;
        }
        if !overlong {
            if end + pending.len() > MAX_LINE {
                overlong = true;
                pending.clear();
            } else {
                pending.splice(0..0, bytes[..end].iter().copied());
            }
        }
        position = start;
    };
    Ok(match complete {
        Some(offset) => Cursor {
            offset,
            turn,
            partial: Vec::new(),
            overlong: false,
        },
        // The whole file is one line still being written: read it forwards.
        None if turn.is_some() => Cursor {
            offset: 0,
            turn,
            partial: Vec::new(),
            overlong: false,
        },
        // A line longer than the search is no boundary; skip to its end.
        None => Cursor {
            offset: size,
            turn,
            partial: Vec::new(),
            overlong: true,
        },
    })
}

/// Newline positions in `bytes`, last first.
fn memrchr_iter(bytes: &[u8]) -> impl Iterator<Item = usize> + '_ {
    bytes
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, byte)| **byte == b'\n')
        .map(|(index, _)| index)
}

/// Reads what was appended since the last read, up to [`MAX_READ`].
fn advance(cursor: &mut Cursor, file: &File, size: u64) -> io::Result<()> {
    let end = size.min(cursor.offset.saturating_add(MAX_READ));
    if cursor.offset >= end {
        return Ok(());
    }
    let mut chunk = vec![0; CHUNK];
    while cursor.offset < end {
        let length = (end - cursor.offset).min(CHUNK as u64) as usize;
        file.read_exact_at(&mut chunk[..length], cursor.offset)?;
        cursor.offset += length as u64;
        let mut lines = chunk[..length].split(|byte| *byte == b'\n');
        let mut segment = lines.next().unwrap_or_default();
        for next in lines {
            // `segment` ends a line.
            if !cursor.overlong && cursor.partial.len() + segment.len() <= MAX_LINE {
                cursor.partial.extend_from_slice(segment);
                if let Some(turn) = boundary(&cursor.partial) {
                    cursor.turn = Some(turn);
                }
            }
            cursor.partial.clear();
            cursor.overlong = false;
            segment = next;
        }
        // `segment` continues past this chunk.
        if !cursor.overlong {
            if cursor.partial.len() + segment.len() > MAX_LINE {
                cursor.partial.clear();
                cursor.overlong = true;
            } else {
                cursor.partial.extend_from_slice(segment);
            }
        }
    }
    // A long line read once is not kept room for in every file followed.
    cursor.partial.shrink_to(CHUNK);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn event(kind: &str) -> String {
        format!(
            "{{\"timestamp\":\"t\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"{kind}\",\"turn_id\":\"x\"}}}}\n"
        )
    }

    fn content(text: &str) -> String {
        serde_json::json!({"type":"response_item","payload":{"type":"message","text":text}})
            .to_string()
            + "\n"
    }

    fn observed(rollouts: &Rollouts, file: &File) -> Option<Turn> {
        rollouts.turn(file, "rollout-test.jsonl".into())
    }

    #[test]
    fn the_last_boundary_is_the_state() {
        let mut file = tempfile::tempfile().unwrap();
        let rollouts = Rollouts::default();
        file.write_all(content("session").as_bytes()).unwrap();
        assert_eq!(observed(&rollouts, &file), Some(Turn::Ended), "no turn yet");
        file.write_all(event("task_started").as_bytes()).unwrap();
        file.write_all(content("working").as_bytes()).unwrap();
        assert_eq!(observed(&rollouts, &file), Some(Turn::Running(None)));
        file.write_all(event("task_complete").as_bytes()).unwrap();
        assert_eq!(observed(&rollouts, &file), Some(Turn::Ended));
        file.write_all(event("task_started").as_bytes()).unwrap();
        assert_eq!(observed(&rollouts, &file), Some(Turn::Running(None)));
        file.write_all(event("turn_aborted").as_bytes()).unwrap();
        assert_eq!(observed(&rollouts, &file), Some(Turn::Ended));
    }

    /// A file first seen mid-turn, far past its boundary, reads as working, and
    /// content that only mentions a boundary is not one.
    #[test]
    fn a_file_first_seen_is_read_back_to_its_last_boundary() {
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(event("task_complete").as_bytes()).unwrap();
        file.write_all(event("task_started").as_bytes()).unwrap();
        let long = "x".repeat(3 * CHUNK);
        for _ in 0..4 {
            file.write_all(content(&long).as_bytes()).unwrap();
        }
        file.write_all(content("\"task_complete\"").as_bytes())
            .unwrap();
        assert_eq!(
            observed(&Rollouts::default(), &file),
            Some(Turn::Running(None))
        );
    }

    /// A boundary still being written when a read reaches the end of the file
    /// is read once it is complete, whichever way it was first met.
    #[test]
    fn a_line_being_written_is_read_once_complete() {
        let line = event("task_started");
        let (head, tail) = line.split_at(20);
        let mut file = tempfile::tempfile().unwrap();
        let rollouts = Rollouts::default();
        file.write_all(event("task_complete").as_bytes()).unwrap();
        file.write_all(head.as_bytes()).unwrap();
        assert_eq!(observed(&rollouts, &file), Some(Turn::Ended));
        file.write_all(tail.as_bytes()).unwrap();
        assert_eq!(observed(&rollouts, &file), Some(Turn::Running(None)));

        let mut file = tempfile::tempfile().unwrap();
        let rollouts = Rollouts::default();
        file.write_all(head.as_bytes()).unwrap();
        assert_eq!(observed(&rollouts, &file), Some(Turn::Ended));
        file.write_all(tail.as_bytes()).unwrap();
        assert_eq!(observed(&rollouts, &file), Some(Turn::Running(None)));
    }

    #[test]
    fn a_line_too_long_to_be_a_boundary_is_skipped() {
        let mut file = tempfile::tempfile().unwrap();
        let rollouts = Rollouts::default();
        file.write_all(event("task_started").as_bytes()).unwrap();
        let huge = format!("\"task_complete\"{}", "x".repeat(MAX_LINE));
        file.write_all(content(&huge).as_bytes()).unwrap();
        assert_eq!(observed(&rollouts, &file), Some(Turn::Running(None)));
        file.write_all(content(&huge).as_bytes()).unwrap();
        file.write_all(event("task_complete").as_bytes()).unwrap();
        assert_eq!(observed(&rollouts, &file), Some(Turn::Ended));
    }

    #[test]
    fn a_file_rewritten_shorter_is_read_again() {
        let mut file = tempfile::tempfile().unwrap();
        let rollouts = Rollouts::default();
        file.write_all(event("task_started").as_bytes()).unwrap();
        file.write_all(content(&"x".repeat(1024)).as_bytes())
            .unwrap();
        assert_eq!(observed(&rollouts, &file), Some(Turn::Running(None)));
        file.set_len(0).unwrap();
        file.write_all_at(event("task_complete").as_bytes(), 0)
            .unwrap();
        assert_eq!(observed(&rollouts, &file), Some(Turn::Ended));
    }

    #[test]
    fn any_thread_mid_turn_makes_the_process_work() {
        use ActivityState::*;
        assert_eq!(merge(None, Idle), Idle);
        assert_eq!(merge(Some(Idle), Working), Working);
        assert_eq!(merge(Some(Working), Unknown), Working);
        assert_eq!(merge(Some(Idle), Unknown), Unknown);
        assert_eq!(merge(Some(Unknown), Idle), Unknown);
    }

    /// A turn a crash left unfinished is not the work of the process that
    /// resumed its thread, however long ago the crash was.
    #[test]
    fn a_turn_begun_before_the_process_is_not_its_work() {
        let started = Some(1_000_000);
        let running = |at| activity(Some(Turn::Running(at)), started);
        assert_eq!(running(Some(900_000)), ActivityState::Idle);
        assert_eq!(running(Some(999_995)), ActivityState::Working, "slack");
        assert_eq!(running(Some(1_000_050)), ActivityState::Working);
        assert_eq!(running(None), ActivityState::Working, "unstamped");
        assert_eq!(
            activity(Some(Turn::Running(Some(0))), None),
            ActivityState::Working,
            "start unknown"
        );
        assert_eq!(activity(None, started), ActivityState::Unknown);

        let mut file = tempfile::tempfile().unwrap();
        file.write_all(
            b"{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\",\"started_at\":1758000000}}\n",
        )
        .unwrap();
        assert_eq!(
            observed(&Rollouts::default(), &file),
            Some(Turn::Running(Some(1_758_000_000)))
        );
    }

    /// A record from before turns stated their start is dated by its own
    /// time, and a stamp of an unexpected form still leaves a boundary.
    #[test]
    fn a_turn_without_a_start_is_dated_by_its_record() {
        let started = |line: &str| match boundary(line.as_bytes()) {
            Some(Turn::Running(at)) => at,
            other => panic!("{other:?}"),
        };
        assert_eq!(
            started(
                r#"{"timestamp":"2026-03-01T12:34:56.789Z","type":"event_msg","payload":{"type":"task_started"}}"#
            ),
            Some(1_772_368_496)
        );
        assert_eq!(
            started(
                r#"{"timestamp":"2026-03-01T12:34:56Z","type":"event_msg","payload":{"type":"task_started","started_at":"soon"}}"#
            ),
            Some(1_772_368_496)
        );
        assert_eq!(
            started(r#"{"type":"event_msg","payload":{"type":"task_started","started_at":null}}"#),
            None
        );
        assert_eq!(unix_seconds("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(unix_seconds("2000-02-29T23:59:59Z"), Some(951_868_799));
        assert_eq!(unix_seconds("2024-12-31T00:00:00Z"), Some(1_735_603_200));
        assert_eq!(unix_seconds("2026-03-01T12:34:56+02:00"), None);
        assert_eq!(unix_seconds("garbage"), None);
    }

    /// The rollouts a live process writes are found through its descriptors;
    /// one it only reads, or holding none, gives no rollout state.
    #[test]
    fn a_process_is_read_through_the_rollouts_it_writes() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory
            .path()
            .join("rollout-2026-01-01T00-00-00-test.jsonl");
        std::fs::write(&path, event("task_started")).unwrap();
        let append = || OpenOptions::new().append(true).open(&path).unwrap();
        let holding = |stdout: std::process::Stdio| {
            std::process::Command::new("sleep")
                .arg("60")
                .stdin(std::process::Stdio::null())
                .stdout(stdout)
                .spawn()
                .unwrap()
        };
        let rollouts = Rollouts::default();
        let mut writer = holding(append().into());
        let pid = writer.id() as i32;
        assert_eq!(rollouts.state(pid, None), Some(ActivityState::Working));
        append()
            .write_all(event("task_complete").as_bytes())
            .unwrap();
        assert_eq!(rollouts.state(pid, None), Some(ActivityState::Idle));
        writer.kill().unwrap();
        writer.wait().unwrap();

        let mut reader = holding(File::open(&path).unwrap().into());
        assert_eq!(rollouts.state(reader.id() as i32, None), None);
        reader.kill().unwrap();
        reader.wait().unwrap();

        let mut other = holding(std::process::Stdio::null());
        assert_eq!(rollouts.state(other.id() as i32, None), None);
        other.kill().unwrap();
        other.wait().unwrap();
    }
}
