//! Live writers of an existing Codex thread.
//!
//! Before this daemon launches a server of its own on a thread, any process
//! already writing it would become a second writer beside that server: a
//! Codex CLI, the desktop app's server, or a listener an earlier daemon or
//! another cagents instance left running. Codex makes one process the writer
//! of a loaded thread by holding the thread's writer lock in its account home,
//! so the lock's holder is that writer. Releases without the lock still keep
//! the thread's current rollout open for writing while it is loaded.
use std::{
    io,
    os::{
        fd::AsRawFd,
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::Path,
};

/// Bounds a census of a host that is spawning processes without end.
const MAX_PROCESSES: usize = 65_536;

/// A process with the thread loaded.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Writer {
    pub(super) pid: libc::pid_t,
    pub(super) name: String,
    /// Whether this daemon started it, as a chat's server or a terminal's CLI.
    pub(super) hosted: bool,
}

/// Find the process writing `thread`, whose writer lock is at `lock`.
///
/// An unreadable lock table or process list is an error, never an absent
/// writer. A process whose descriptors the kernel hides from this account, as
/// it does a non-dumpable or more privileged one's, is treated as writing
/// nothing in the census; only the lock can name it.
pub(super) fn writer(lock: Option<&Path>, thread: &str) -> Result<Option<Writer>, String> {
    let pid = match lock {
        Some(lock) => lock_holder(lock, &read("/proc/locks")?)?,
        None => None,
    };
    let pid = match pid {
        Some(pid) => Some(pid),
        None => rollout_writer(thread)?,
    };
    Ok(pid.map(|pid| Writer {
        pid,
        name: std::fs::read_to_string(format!("/proc/{pid}/comm"))
            .map(|name| name.trim_end().to_owned())
            .unwrap_or_default(),
        hosted: descends_from(pid, std::process::id() as libc::pid_t),
    }))
}

fn read(path: &str) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|error| format!("Cannot read {path}: {error}"))
}

/// The process holding the lock at `lock`, according to the kernel's `locks`
/// table. Waiters queued behind the holder are not writers.
fn lock_holder(lock: &Path, table: &str) -> Result<Option<libc::pid_t>, String> {
    // Pin the inode and its mount together. Btrfs stat() reports a different
    // device for each subvolume, while /proc/locks uses the superblock device
    // from mountinfo, so metadata.dev() is not the kernel lock identity.
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH | libc::O_CLOEXEC)
        .open(lock)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(format!(
                "Cannot inspect Codex thread writer lock {}: {error}",
                lock.display()
            ));
        }
    };
    let expected_inode = file
        .metadata()
        .map_err(|error| format!("Cannot inspect Codex thread writer lock: {error}"))?
        .ino();
    let (major, minor) = kernel_device(&file)?;
    Ok(table.lines().find_map(|line| {
        let fields: Vec<_> = line.split_whitespace().collect();
        let [_, kind, _, _, pid, identity, ..] = fields[..] else {
            return None;
        };
        if kind == "->" {
            return None;
        }
        let mut identity = identity.split(':');
        let device = (
            u32::from_str_radix(identity.next()?, 16).ok()?,
            u32::from_str_radix(identity.next()?, 16).ok()?,
        );
        let inode: u64 = identity.next()?.parse().ok()?;
        (device == (major, minor) && inode == expected_inode)
            .then(|| pid.parse().ok())
            .flatten()
    }))
}

/// Device used by the kernel lock table for this exact open mount.
fn kernel_device(file: &std::fs::File) -> Result<(u32, u32), String> {
    let info = read(&format!("/proc/self/fdinfo/{}", file.as_raw_fd()))?;
    let mount = info
        .lines()
        .find_map(|line| line.strip_prefix("mnt_id:").map(str::trim))
        .ok_or("Cannot identify the Codex writer lock's mount")?;
    mount_device(mount, &read("/proc/self/mountinfo")?)
        .ok_or_else(|| "Cannot identify the Codex writer lock's kernel device".into())
}

fn mount_device(mount: &str, table: &str) -> Option<(u32, u32)> {
    table.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        if fields.next()? != mount {
            return None;
        }
        fields.next()?;
        let (major, minor) = fields.next()?.split_once(':')?;
        Some((major.parse().ok()?, minor.parse().ok()?))
    })
}

/// A process of this account with one of `thread`'s rollouts open for writing.
fn rollout_writer(thread: &str) -> Result<Option<libc::pid_t>, String> {
    let account = unsafe { libc::geteuid() };
    let own = std::process::id() as libc::pid_t;
    let entries =
        std::fs::read_dir("/proc").map_err(|error| format!("Cannot list processes: {error}"))?;
    for (index, entry) in entries.enumerate() {
        if index >= MAX_PROCESSES {
            return Err("Process census exceeded its safety bound".into());
        }
        let entry = entry.map_err(|error| format!("Cannot list processes: {error}"))?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<libc::pid_t>().ok())
        else {
            continue;
        };
        // A non-dumpable process's entry belongs to root and is passed over.
        if pid == own
            || !entry
                .metadata()
                .is_ok_and(|metadata| metadata.uid() == account)
        {
            continue;
        }
        if writes_rollout(&entry.path(), thread)? {
            return Ok(Some(pid));
        }
    }
    Ok(None)
}

/// Whether the process at `process` has one of `thread`'s rollouts open for
/// writing. A reader, such as a picker listing past sessions, writes nothing.
fn writes_rollout(process: &Path, thread: &str) -> Result<bool, String> {
    let descriptors = match std::fs::read_dir(process.join("fd")) {
        Ok(descriptors) => descriptors,
        Err(error) if exited(&error) || error.kind() == io::ErrorKind::PermissionDenied => {
            return Ok(false);
        }
        Err(error) => {
            return Err(format!(
                "Cannot inspect open files of {}: {error}",
                process.display()
            ));
        }
    };
    for descriptor in descriptors {
        let descriptor = match descriptor {
            Ok(descriptor) => descriptor,
            Err(error) if exited(&error) => return Ok(false),
            Err(error) => return Err(format!("Cannot inspect open files: {error}")),
        };
        let Ok(target) = std::fs::read_link(descriptor.path()) else {
            continue;
        };
        if rollout_of(&target, thread)
            && opened_for_writing(&process.join("fdinfo").join(descriptor.file_name()))
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Whether `target` is a rollout of `thread`: its first segment, named
/// `rollout-<time>-<thread>.jsonl`, or a later one, `…-<thread>_<segment>.jsonl`.
fn rollout_of(target: &Path, thread: &str) -> bool {
    let Some(stem) = target
        .file_name()
        .and_then(|name| name.to_str())
        // procfs retains an unlinked file's original name with this suffix.
        // Its open writer remains live until the descriptor is closed.
        .map(|name| name.strip_suffix(" (deleted)").unwrap_or(name))
        .and_then(|name| name.strip_prefix("rollout-"))
        .and_then(|name| name.strip_suffix(".jsonl"))
    else {
        return false;
    };
    let named = |rest: &str| {
        rest.strip_prefix(thread)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('_'))
    };
    stem.match_indices('-')
        .any(|(index, _)| named(&stem[index + 1..]))
}

fn opened_for_writing(info: &Path) -> bool {
    std::fs::read_to_string(info).is_ok_and(|info| {
        info.lines()
            .find_map(|line| line.strip_prefix("flags:"))
            .and_then(|flags| i32::from_str_radix(flags.trim(), 8).ok())
            .is_some_and(|flags| flags & libc::O_ACCMODE != libc::O_RDONLY)
    })
}

/// A process that exited after being listed.
fn exited(error: &io::Error) -> bool {
    matches!(error.raw_os_error(), Some(libc::ENOENT | libc::ESRCH))
}

/// Whether `pid` is `ancestor` or one of its descendants.
fn descends_from(mut pid: libc::pid_t, ancestor: libc::pid_t) -> bool {
    // Bounded, as a parent chain read across exits may briefly loop.
    for _ in 0..64 {
        if pid == ancestor {
            return true;
        }
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            return false;
        };
        // The command name may hold spaces and parentheses; fields follow its last ')'.
        let Some(parent) = stat
            .rsplit_once(')')
            .and_then(|(_, fields)| fields.split_whitespace().nth(1))
            .and_then(|parent| parent.parse().ok())
        else {
            return false;
        };
        if parent <= 1 {
            return false;
        }
        pid = parent;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// One process holding `file` open, so killing it closes the file.
    fn holder(file: &Path, redirection: &str) -> std::process::Child {
        std::process::Command::new("/bin/sh")
            .args(["-c", &format!("exec /bin/sleep 30 3{redirection}\"$0\"")])
            .arg(file)
            .spawn()
            .unwrap()
    }

    fn eventually(mut found: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !found() {
            assert!(Instant::now() < deadline, "never observed");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn a_rollout_names_its_thread_in_every_segment() {
        let thread = "01a13c47-0fad-7002-9d26-7ba2fac32335";
        for (name, expected) in [
            (format!("rollout-2026-10-14T09-00-00-{thread}.jsonl"), true),
            (
                format!("rollout-2026-10-14T09-00-00-{thread}.jsonl (deleted)"),
                true,
            ),
            (
                format!(
                    "rollout-2026-11-04T09-00-00-{thread}_01a1a9fb-0000-7000-8000-000000000000.jsonl"
                ),
                true,
            ),
            (
                format!("rollout-2026-10-14T09-00-00-{thread}0.jsonl"),
                false,
            ),
            (format!("notes-{thread}.jsonl"), false),
            (format!("rollout-2026-10-14T09-00-00-{thread}.json"), false),
            (
                "rollout-2026-10-14T09-00-00-01a0df4a-0000-7000-8000-000000000000.jsonl".into(),
                false,
            ),
        ] {
            assert_eq!(rollout_of(Path::new(&name), thread), expected, "{name}");
        }
    }

    #[test]
    fn the_lock_holder_is_read_from_the_kernel_table_and_waiters_are_not() {
        let directory = tempfile::tempdir().unwrap();
        let lock = directory.path().join("thread.lock");
        std::fs::write(&lock, b"").unwrap();
        let file = std::fs::metadata(&lock).unwrap();
        let (major, minor) = kernel_device(&std::fs::File::open(&lock).unwrap()).unwrap();
        let identity = format!("{major:02x}:{minor:02x}:{}", file.ino());
        let table = format!(
            "1: FLOCK  ADVISORY  WRITE 4242 00:00:1 0 EOF\n\
             2: FLOCK  ADVISORY  WRITE 31337 {identity} 0 EOF\n\
             2: -> FLOCK  ADVISORY  WRITE 7 {identity} 0 EOF\n"
        );
        assert_eq!(lock_holder(&lock, &table).unwrap(), Some(31337));
        let waiting = format!("1: -> FLOCK  ADVISORY  WRITE 7 {identity} 0 EOF\n");
        assert_eq!(lock_holder(&lock, &waiting).unwrap(), None);
        assert_eq!(
            lock_holder(&directory.path().join("absent.lock"), &table).unwrap(),
            None
        );

        // The live table reports a real holder: here, this process.
        let held = std::fs::File::open(&lock).unwrap();
        use std::os::fd::AsRawFd;
        assert_eq!(unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_EX) }, 0);
        let found = writer(Some(&lock), "unrelated").unwrap().unwrap();
        assert_eq!(found.pid, std::process::id() as libc::pid_t);
        assert!(found.hosted);
        drop(held);
        assert_eq!(
            lock_holder(&lock, &read("/proc/locks").unwrap()).unwrap(),
            None
        );
    }

    #[test]
    fn a_lock_uses_its_exact_mounts_kernel_device_including_btrfs_subvolumes() {
        let mounts = "31 20 0:30 /@home /home rw - btrfs /dev/nvme0n1p2 rw\n\
                      42 20 0:30 /@tmp /var/tmp rw - btrfs /dev/nvme0n1p2 rw\n\
                      43 20 0:58 / /tmp rw - tmpfs tmpfs rw\n";
        // The Btrfs subvolume's stat device can be 0:57, while this mount
        // shares the superblock device 0:30 used in /proc/locks.
        assert_eq!(mount_device("42", mounts), Some((0, 30)));
        assert_eq!(mount_device("43", mounts), Some((0, 58)));
        assert_eq!(mount_device("4", mounts), None);
        assert_eq!(mount_device("42", "42 20 not-a-device / /tmp"), None);
    }

    #[test]
    fn only_a_process_writing_the_threads_rollout_is_its_writer() {
        let directory = tempfile::tempdir().unwrap();
        let thread = uuid::Uuid::new_v4().to_string();
        let other = uuid::Uuid::new_v4().to_string();
        let rollout = |name: String| {
            let path = directory.path().join(name);
            std::fs::write(&path, b"").unwrap();
            path
        };
        assert_eq!(writer(None, &thread).unwrap(), None);

        let mut readers = [
            holder(
                &rollout(format!("rollout-2026-11-08T12-00-00-{other}.jsonl")),
                ">>",
            ),
            holder(&rollout(format!("notes-{thread}.jsonl")), ">>"),
            holder(
                &rollout(format!("rollout-2026-11-08T12-00-00-{thread}.jsonl")),
                "<",
            ),
        ];
        let last = readers[2].id() as libc::pid_t;
        eventually(|| {
            std::fs::read_dir(format!("/proc/{last}/fd")).is_ok_and(|fds| fds.count() > 3)
        });
        assert_eq!(
            writer(None, &thread).unwrap(),
            None,
            "readers write nothing"
        );

        let segment = rollout(format!(
            "rollout-2026-11-09T12-00-00-{thread}_{}.jsonl",
            uuid::Uuid::new_v4()
        ));
        let mut loaded = holder(&segment, ">>");
        let mut found = None;
        eventually(|| {
            found = writer(None, &thread).unwrap();
            found.is_some()
        });
        let found = found.unwrap();
        assert_eq!(found.pid, loaded.id() as libc::pid_t);
        assert_eq!(found.name, "sleep");
        assert!(found.hosted, "a child of this process is one it started");
        std::fs::remove_file(&segment).unwrap();
        assert_eq!(writer(None, &thread).unwrap().unwrap().pid, found.pid);
        for child in readers.iter_mut().chain([&mut loaded]) {
            child.kill().unwrap();
            child.wait().unwrap();
        }
        assert_eq!(writer(None, &thread).unwrap(), None);
    }
}
