//! Live writers of an existing Codex thread.
//!
//! Before this daemon launches a server of its own on a thread, any process
//! already writing it would become a second writer beside that server: a
//! Codex CLI, the desktop app's server, or a listener an earlier daemon or
//! another cagents instance left running. Codex makes one process the writer
//! of a loaded thread by holding the thread's writer lock in its account home,
//! so the lock's holder is that writer. Releases without the lock still keep
//! the thread's current rollout open for writing while it is loaded.
//!
//! Linux names the lock's holder from the kernel's lock table and finds a
//! rollout's writer by walking `/proc`. Darwin has no lock table, so there a
//! held lock is told by a refused probe and its holder named by a census of
//! this account's processes for one that has the lock open; a lock held by a
//! process the census cannot see is an error rather than an absent writer.
use std::{
    io,
    os::{
        fd::AsRawFd,
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::Path,
};

/// Bounds a census of a host that is spawning processes without end.
#[cfg(target_os = "linux")]
const MAX_PROCESSES: usize = 65_536;

/// A process with the thread loaded.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Writer {
    pub(super) pid: libc::pid_t,
}

/// Find the process writing `thread`, whose writer lock is at `lock`.
///
/// An unreadable lock table or process list is an error, never an absent
/// writer. A process whose descriptors the kernel hides from this account, as
/// it does a non-dumpable or more privileged one's, is treated as writing
/// nothing in the census; only the lock can name it.
pub(super) fn writer(lock: Option<&Path>, thread: &str) -> io::Result<Option<Writer>> {
    let pid = match lock {
        Some(lock) => held_lock_holder(lock)?,
        None => None,
    };
    let pid = match pid {
        Some(pid) => Some(pid),
        None => rollout_writer(thread).map_err(io::Error::other)?,
    };
    Ok(pid.map(|pid| Writer { pid }))
}

#[cfg(target_os = "linux")]
fn lock_table() -> Result<String, String> {
    #[cfg(test)]
    if let Some(table) = LOCK_TABLE.with(|slot| {
        slot.borrow().as_ref().map(|table| {
            let read = table.reads.get();
            table.reads.set(read + 1);
            // The last table stands for every later read.
            table.values[read.min(table.values.len() - 1)].clone()
        })
    }) {
        return Ok(table);
    }
    read("/proc/locks")
}

#[cfg(all(test, target_os = "linux"))]
std::thread_local! {
    static LOCK_TABLE: std::cell::RefCell<Option<TestLockTable>> = const { std::cell::RefCell::new(None) };
}

#[cfg(all(test, target_os = "linux"))]
struct TestLockTable {
    values: Vec<String>,
    reads: std::rc::Rc<std::cell::Cell<usize>>,
}

/// A synchronous request's controlled table input, never shared with another
/// test thread. The Rc also prevents moving its restoration guard to one.
#[cfg(all(test, target_os = "linux"))]
pub(super) struct LockTableScope {
    previous: Option<TestLockTable>,
    reads: std::rc::Rc<std::cell::Cell<usize>>,
}

#[cfg(all(test, target_os = "linux"))]
impl LockTableScope {
    pub(super) fn new(value: String) -> Self {
        Self::reads_of(vec![value])
    }

    /// Successive reads see successive tables; the last one repeats.
    pub(super) fn reads_of(values: Vec<String>) -> Self {
        assert!(!values.is_empty());
        let reads = std::rc::Rc::new(std::cell::Cell::new(0));
        let previous = LOCK_TABLE.with(|slot| {
            slot.replace(Some(TestLockTable {
                values,
                reads: reads.clone(),
            }))
        });
        Self { previous, reads }
    }

    pub(super) fn reads(&self) -> usize {
        self.reads.get()
    }
}

#[cfg(all(test, target_os = "linux"))]
impl Drop for LockTableScope {
    fn drop(&mut self) {
        LOCK_TABLE.with(|slot| slot.replace(self.previous.take()));
    }
}

#[cfg(all(test, target_os = "macos"))]
std::thread_local! {
    static HIDDEN_HOLDERS: std::cell::RefCell<Option<std::rc::Rc<std::cell::Cell<usize>>>> =
        const { std::cell::RefCell::new(None) };
}

/// Darwin has no lock table, so a scope can only stand for an empty one: it
/// hides every holder from the census that names one, as a table missing the
/// holder's row hides it on Linux. A held lock then has no holder to name.
#[cfg(all(test, target_os = "macos"))]
pub(super) struct LockTableScope {
    previous: Option<std::rc::Rc<std::cell::Cell<usize>>>,
    reads: std::rc::Rc<std::cell::Cell<usize>>,
}

#[cfg(all(test, target_os = "macos"))]
impl LockTableScope {
    pub(super) fn new(value: String) -> Self {
        assert!(value.is_empty(), "Darwin has no lock table rows to supply");
        let reads = std::rc::Rc::new(std::cell::Cell::new(0));
        let previous = HIDDEN_HOLDERS.with(|slot| slot.replace(Some(reads.clone())));
        Self { previous, reads }
    }

    pub(super) fn reads(&self) -> usize {
        self.reads.get()
    }
}

#[cfg(all(test, target_os = "macos"))]
impl Drop for LockTableScope {
    fn drop(&mut self) {
        HIDDEN_HOLDERS.with(|slot| slot.replace(self.previous.take()));
    }
}

#[cfg(target_os = "linux")]
fn read(path: &str) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|error| format!("Cannot read {path}: {error}"))
}

/// The holder of `lock`. A held lock missing from one read of the table is
/// read for again: see `crate::flock::proven_owners` for why a read can skip
/// it.
#[cfg(target_os = "linux")]
fn held_lock_holder(lock: &Path) -> io::Result<Option<libc::pid_t>> {
    let mut reads = 1;
    loop {
        match lock_holder(lock, &lock_table().map_err(io::Error::other)?) {
            Err(error)
                if error.kind() == io::ErrorKind::WouldBlock
                    && reads < crate::flock::INDEX_READS =>
            {
                reads += 1;
            }
            result => return result,
        }
    }
}

/// The process holding the lock at `lock`, according to the kernel's `locks`
/// table. Waiters queued behind the holder are not writers.
#[cfg(target_os = "linux")]
fn lock_holder(lock: &Path, table: &str) -> io::Result<Option<libc::pid_t>> {
    lock_holder_before_check(lock, table, || Ok(()))
}

#[cfg(target_os = "linux")]
fn lock_holder_before_check(
    lock: &Path,
    table: &str,
    before_check: impl FnOnce() -> io::Result<()>,
) -> io::Result<Option<libc::pid_t>> {
    let inspect = |error| inspect_error(lock, error);
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
        Err(error) => return Err(inspect(error)),
    };
    let pinned = file.metadata().map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("Cannot inspect Codex thread writer lock: {error}"),
        )
    })?;
    if !pinned.is_file() {
        return Err(inspect(io::Error::from_raw_os_error(libc::EINVAL)));
    }
    let expected_inode = pinned.ino();
    let (major, minor) = kernel_device(&file).map_err(io::Error::other)?;
    if let Some(pid) = table.lines().find_map(|line| {
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
    }) {
        return Ok(Some(pid));
    }

    // No matching table row is not proof that this inode is unheld. Reopen
    // the pinned regular file: dup() would share its open file description
    // and could acquire a lock already owned through that description.
    // Read-only is deliberate; a filesystem that requires write access for
    // exclusive flock cannot supply this absence proof.
    let probe = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(format!("/proc/self/fd/{}", file.as_raw_fd()))
        .map_err(&inspect)?;
    let probed = probe.metadata().map_err(&inspect)?;
    if !same_lock(&pinned, &probed) {
        return Err(inspect(io::Error::from_raw_os_error(libc::ESTALE)));
    }
    unheld_while_probed(lock, &pinned, probe, before_check).map_err(inspect)?;
    Ok(None)
}

/// The process holding the lock at `lock`. Darwin keeps no table of locks, so
/// a probe on a description of its own tells whether the lock is held, and a
/// census names a process of this account that has the lock open. With
/// several open, the one named may be waiting behind the holder rather than
/// holding it; either is a live writer of the thread. A held lock that no
/// process the census can see has open, as a more privileged process's
/// descriptors are hidden, is an error, never an absent writer.
#[cfg(target_os = "macos")]
fn held_lock_holder(lock: &Path) -> io::Result<Option<libc::pid_t>> {
    lock_holder_before_check(lock, || Ok(()))
}

#[cfg(target_os = "macos")]
fn lock_holder_before_check(
    lock: &Path,
    before_check: impl FnOnce() -> io::Result<()>,
) -> io::Result<Option<libc::pid_t>> {
    let inspect = |error| inspect_error(lock, error);
    // Darwin has no O_PATH to pin the file without opening it, so check its
    // kind before opening it at all, then that the open reached that file.
    let pinned = match std::fs::metadata(lock) {
        Ok(pinned) => pinned,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(inspect(error)),
    };
    if !pinned.is_file() {
        return Err(inspect(io::Error::from_raw_os_error(libc::EINVAL)));
    }
    // A description of this probe's own, so it cannot acquire a lock already
    // owned through one this process shares. Read-only, as on Linux.
    let probe = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(lock)
    {
        Ok(probe) => probe,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(inspect(error)),
    };
    let probed = probe.metadata().map_err(&inspect)?;
    if !same_lock(&pinned, &probed) {
        return Err(inspect(io::Error::from_raw_os_error(libc::ESTALE)));
    }
    let refused = match unheld_while_probed(lock, &pinned, probe, before_check) {
        Ok(()) => return Ok(None),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => error,
        Err(error) => return Err(inspect(error)),
    };
    #[cfg(test)]
    if let Some(reads) = HIDDEN_HOLDERS.with(|slot| slot.borrow().clone()) {
        reads.set(reads.get() + 1);
        return Err(inspect(refused));
    }
    let (device, inode) = (pinned.dev(), pinned.ino());
    match crate::process::account_process_holding(|file| {
        file.device == device && file.inode == inode
    }) {
        Ok(Some(pid)) => Ok(Some(pid as libc::pid_t)),
        Ok(None) => Err(inspect(refused)),
        Err(error) => Err(inspect(error)),
    }
}

/// Proves the lock at `lock`, whose file is `pinned`, unheld for an instant by
/// taking it through `probe`, a description of its own, then releasing it. A
/// held lock refuses with `WouldBlock`.
fn unheld_while_probed(
    lock: &Path,
    pinned: &std::fs::Metadata,
    probe: std::fs::File,
    before_check: impl FnOnce() -> io::Result<()>,
) -> io::Result<()> {
    // SAFETY: flock takes a descriptor `probe` owns and scalar flags.
    if unsafe { libc::flock(probe.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // Check the original pathname while the independent probe is held, then
    // release it on every path. It is an observation, not a reservation that
    // can exclude an external writer starting after this function returns.
    let checked = before_check().and_then(|()| {
        let current = std::fs::metadata(lock)?;
        if same_lock(pinned, &current) {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(libc::ESTALE))
        }
    });
    // SAFETY: as above; this releases the lock the probe took.
    let released = if unsafe { libc::flock(probe.as_raw_fd(), libc::LOCK_UN) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    };
    drop(probe);
    checked?;
    released
}

fn same_lock(expected: &std::fs::Metadata, actual: &std::fs::Metadata) -> bool {
    actual.is_file() && expected.dev() == actual.dev() && expected.ino() == actual.ino()
}

fn inspect_error(lock: &Path, error: io::Error) -> io::Error {
    io::Error::new(
        error.kind(),
        format!(
            "Cannot inspect Codex thread writer lock {}: {error}",
            lock.display()
        ),
    )
}

/// Device used by the kernel lock table for this exact open mount.
#[cfg(target_os = "linux")]
fn kernel_device(file: &std::fs::File) -> Result<(u32, u32), String> {
    let info = read(&format!("/proc/self/fdinfo/{}", file.as_raw_fd()))?;
    let mount = info
        .lines()
        .find_map(|line| line.strip_prefix("mnt_id:").map(str::trim))
        .ok_or("Cannot identify the Codex writer lock's mount")?;
    mount_device(mount, &read("/proc/self/mountinfo")?)
        .ok_or_else(|| "Cannot identify the Codex writer lock's kernel device".into())
}

#[cfg(target_os = "linux")]
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
#[cfg(target_os = "linux")]
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
#[cfg(target_os = "linux")]
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

/// A process of this account, other than this one, with one of `thread`'s
/// rollouts open for writing. A reader writes nothing. A process whose
/// descriptors Darwin hides from this account is passed over, as on Linux.
#[cfg(target_os = "macos")]
fn rollout_writer(thread: &str) -> Result<Option<libc::pid_t>, String> {
    crate::process::account_process_holding(|file| file.writable && rollout_of(&file.path, thread))
        .map(|pid| pid.map(|pid| pid as libc::pid_t))
        .map_err(|error| format!("Cannot take a census of processes: {error}"))
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

#[cfg(target_os = "linux")]
fn opened_for_writing(info: &Path) -> bool {
    std::fs::read_to_string(info).is_ok_and(|info| {
        info.lines()
            .find_map(|line| line.strip_prefix("flags:"))
            .and_then(|flags| i32::from_str_radix(flags.trim(), 8).ok())
            .is_some_and(|flags| flags & libc::O_ACCMODE != libc::O_RDONLY)
    })
}

/// A process that exited after being listed.
#[cfg(target_os = "linux")]
fn exited(error: &io::Error) -> bool {
    matches!(error.raw_os_error(), Some(libc::ENOENT | libc::ESRCH))
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

    /// Whether `pid` has opened the file its shell redirected.
    fn has_opened(pid: libc::pid_t) -> bool {
        #[cfg(target_os = "linux")]
        return std::fs::read_dir(format!("/proc/{pid}/fd")).is_ok_and(|fds| fds.count() > 3);
        #[cfg(target_os = "macos")]
        // The shell opens it before it becomes sleep.
        return crate::process::program_of(pid as u32)
            .is_ok_and(|(executable, _)| executable == Path::new("/bin/sleep"));
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

    /// The absence probe, with no lock table rows to name a holder from.
    fn probe_absence(
        lock: &Path,
        before_check: impl FnOnce() -> io::Result<()>,
    ) -> io::Result<Option<libc::pid_t>> {
        #[cfg(target_os = "linux")]
        return lock_holder_before_check(lock, "", before_check);
        #[cfg(target_os = "macos")]
        return lock_holder_before_check(lock, before_check);
    }

    #[cfg(target_os = "linux")]
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
        match writer(Some(&lock), "unrelated") {
            Ok(Some(found)) => assert_eq!(found.pid, std::process::id() as libc::pid_t),
            Err(failure) => assert_eq!(failure.kind(), io::ErrorKind::WouldBlock),
            Ok(None) => panic!("a real exclusive lock was reported absent"),
        }
        // Another test may fork before exec while this file is open. Those
        // inherited descriptors share its open file description, just like
        // this duplicate, so dropping our original alone need not unlock it.
        let inherited = held.try_clone().unwrap();
        assert_eq!(unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_UN) }, 0);
        drop(held);
        assert_eq!(
            lock_holder(&lock, &read("/proc/locks").unwrap()).unwrap(),
            None
        );
        drop(inherited);
    }

    fn independent_flock(path: &Path) -> io::Result<()> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(path)?;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(io::Error::last_os_error());
        }
        assert_eq!(unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) }, 0);
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_missing_lock_table_row_cannot_hide_real_contention() {
        let directory = tempfile::tempdir().unwrap();
        let lock = directory.path().join("thread.lock");
        std::fs::write(&lock, b"SDK contents remain unchanged").unwrap();
        let held = std::fs::File::open(&lock).unwrap();
        assert_eq!(unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_EX) }, 0);
        let inherited = held.try_clone().unwrap();
        assert_eq!(
            independent_flock(&lock).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        let failure = lock_holder(&lock, "").unwrap_err();
        assert_eq!(failure.kind(), io::ErrorKind::WouldBlock);
        assert_eq!(
            failure.to_string(),
            format!(
                "Cannot inspect Codex thread writer lock {}: {}",
                lock.display(),
                io::Error::from_raw_os_error(libc::EWOULDBLOCK)
            )
        );
        // The kernel can skip a held row in one read of the table; the
        // holder is read for again, a bounded number of times.
        let (major, minor) = kernel_device(&held).unwrap();
        let identity = format!("{major:02x}:{minor:02x}:{}", held.metadata().unwrap().ino());
        let row = format!("1: FLOCK  ADVISORY  WRITE 31337 {identity} 0 EOF\n");
        let tables = LockTableScope::reads_of(vec![String::new(), row]);
        assert_eq!(held_lock_holder(&lock).unwrap(), Some(31337));
        assert_eq!(tables.reads(), 2);
        drop(tables);
        let tables = LockTableScope::new(String::new());
        assert_eq!(
            held_lock_holder(&lock).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        assert_eq!(tables.reads(), crate::flock::INDEX_READS);
        drop(tables);
        assert_eq!(
            independent_flock(&lock).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );

        assert_eq!(unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_UN) }, 0);
        drop(held);
        assert_eq!(lock_holder(&lock, "").unwrap(), None);
        independent_flock(&lock).unwrap();
        assert_eq!(
            std::fs::read(&lock).unwrap(),
            b"SDK contents remain unchanged"
        );
        drop(inherited);
    }

    #[test]
    fn an_absence_probe_rejects_changed_paths_and_releases_its_lock() {
        for replace in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let lock = directory.path().join("thread.lock");
            std::fs::write(&lock, b"original").unwrap();
            let original = std::fs::File::open(&lock).unwrap();
            let failure = probe_absence(&lock, || {
                std::fs::remove_file(&lock)?;
                if replace {
                    std::fs::write(&lock, b"replacement")?;
                }
                Ok(())
            })
            .unwrap_err();
            assert_eq!(
                failure.kind(),
                if replace {
                    io::Error::from_raw_os_error(libc::ESTALE).kind()
                } else {
                    io::ErrorKind::NotFound
                }
            );
            // The original's description is not the probe's, so it takes
            // the lock only once the probe has released it.
            assert_eq!(
                unsafe { libc::flock(original.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
                0
            );
        }
    }

    #[test]
    fn an_absence_probe_preserves_symlinks_and_fails_closed_on_io_and_shape_errors() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("target");
        std::fs::write(&target, b"unchanged").unwrap();
        let lock = directory.path().join("thread.lock");
        std::os::unix::fs::symlink(&target, &lock).unwrap();
        assert_eq!(probe_absence(&lock, || Ok(())).unwrap(), None);
        assert!(
            std::fs::symlink_metadata(&lock)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        independent_flock(&target).unwrap();
        let failure =
            probe_absence(&lock, || Err(io::Error::from_raw_os_error(libc::EACCES))).unwrap_err();
        assert_eq!(failure.kind(), io::ErrorKind::PermissionDenied);
        independent_flock(&target).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"unchanged");
        let replacement = directory.path().join("replacement");
        std::fs::write(&replacement, b"other inode").unwrap();
        let failure = probe_absence(&lock, || {
            std::fs::remove_file(&lock)?;
            std::os::unix::fs::symlink(&replacement, &lock)
        })
        .unwrap_err();
        assert_eq!(
            failure.kind(),
            io::Error::from_raw_os_error(libc::ESTALE).kind()
        );
        independent_flock(&target).unwrap();
        independent_flock(&replacement).unwrap();

        assert_eq!(
            probe_absence(directory.path(), || Ok(()))
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        let absent = directory.path().join("absent.lock");
        assert_eq!(probe_absence(&absent, || Ok(())).unwrap(), None);
        assert!(!absent.exists(), "inspection must not create an SDK lock");
    }

    #[cfg(target_os = "linux")]
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

    #[cfg(target_os = "macos")]
    #[test]
    fn a_held_lock_names_a_process_with_it_open_or_fails_closed() {
        let directory = tempfile::tempdir().unwrap();
        let lock = directory.path().join("thread.lock");
        std::fs::write(&lock, b"").unwrap();
        assert_eq!(held_lock_holder(&lock).unwrap(), None);

        // This process holds the lock; the census passes over this process,
        // so until another has it open, nothing it can see is the holder.
        let held = std::fs::File::open(&lock).unwrap();
        assert_eq!(unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_EX) }, 0);
        assert_eq!(
            held_lock_holder(&lock).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        let mut opener = holder(&lock, "<");
        let pid = opener.id() as libc::pid_t;
        eventually(|| has_opened(pid));
        assert_eq!(held_lock_holder(&lock).unwrap(), Some(pid));
        assert_eq!(
            writer(Some(&lock), "unrelated").unwrap(),
            Some(Writer { pid })
        );
        let hidden = LockTableScope::new(String::new());
        assert_eq!(
            held_lock_holder(&lock).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        assert_eq!(hidden.reads(), 1);
        drop(hidden);

        // Released, the lock has no holder even while a process has it open.
        assert_eq!(unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_UN) }, 0);
        assert_eq!(held_lock_holder(&lock).unwrap(), None);
        independent_flock(&lock).unwrap();
        opener.kill().unwrap();
        opener.wait().unwrap();
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
        eventually(|| has_opened(last));
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
        std::fs::remove_file(&segment).unwrap();
        assert_eq!(writer(None, &thread).unwrap().unwrap().pid, found.pid);
        for child in readers.iter_mut().chain([&mut loaded]) {
            child.kill().unwrap();
            child.wait().unwrap();
        }
        assert_eq!(writer(None, &thread).unwrap(), None);
    }
}
