//! The processes a conversation's Cagents helper left behind when it died, and
//! ending them for a person who closes that conversation.
//!
//! A helper leads its own session and starts its provider as the leader of a
//! process group inside it. While anything is left in that session or group,
//! the conversation can still run, so Close refuses it
//! ([`crate::chat::runtime::retained_actor_abandoned`] and
//! [`crate::chat::runtime::codex_execution_abandoned`] look for the same
//! members as [`census`]). The person can then end them here, rather than in
//! a shell. One case differs: for a retained Claude actor whose helper or
//! provider PID another process now holds, Close still counts that process's
//! session or group, which this census leaves out, so ending cannot clear it.
//!
//! Membership is the proof that a process is the conversation's. Linux never
//! hands out a PID while a process still uses it as a process-group or
//! session ID, so while the helper's session or the provider's group has a
//! member, its ID is still theirs. Only once one empties can its ID be reused,
//! by a process that is then alive under that PID with another start time;
//! whatever is in a session or group of that ID is then that process's, and
//! not counted at all. A member is proved only when its ID's original process
//! is still that process or no process has that PID, it started no earlier
//! than that original did, and it runs as this user. Anything else in the
//! session or group is named and still holds the conversation, but is never
//! signalled.
//!
//! Ending signals only the processes the person was shown and chose to end,
//! each as it was when shown: its PID and its start time. It pins each with
//! a pidfd and reads it again before signalling it, so a PID that changed
//! hands since fails that reading, and a pidfd can never signal another
//! process. A process found since is shown before anything ends it.
use crate::daemon::holder_registry::Process;
use std::{
    fs::File,
    io,
    os::fd::{AsRawFd, FromRawFd},
    path::Path,
    time::{Duration, Instant},
};

/// A process left in a lost helper's session or its provider's group.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Survivor {
    pub pid: i32,
    pub start: u64,
    pub group: i32,
    pub session: i32,
    /// It has exited and waits for its parent to collect it: it can't run,
    /// but it still occupies its group until it is collected.
    pub zombie: bool,
    /// The executable's short name, as `ps` shows it.
    pub name: String,
    /// Its command line, if it still shows one.
    pub command: String,
    /// Whether membership proves it is this conversation's, so that ending
    /// it is safe (see the module documentation).
    pub proved: bool,
}

/// What `/proc/<pid>/stat` says of one process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Stat {
    zombie: bool,
    group: i32,
    session: i32,
    start: u64,
}

/// Reads one process's stat entry. `None` when it has gone.
fn read_stat(process: &Path) -> Result<Option<Stat>, String> {
    let pid = process.display();
    let stat = match std::fs::read(process.join("stat")) {
        Ok(stat) => stat,
        // A process that exited during the scan holds no group: its entry is
        // gone, or was reaped between opening and reading it.
        Err(error)
            if error.kind() == io::ErrorKind::NotFound
                || error.raw_os_error() == Some(libc::ESRCH) =>
        {
            return Ok(None);
        }
        Err(error) => return Err(format!("Cannot read process {pid}: {error}")),
    };
    let Some(close) = stat.iter().rposition(|&byte| byte == b')') else {
        return Err(format!("Process {pid} has an unreadable identity"));
    };
    let fields = String::from_utf8_lossy(&stat[close + 1..]);
    let fields: Vec<&str> = fields.split_ascii_whitespace().collect();
    let field = |index: usize| fields.get(index).copied();
    let (Some(state), Some(group), Some(session)) = (
        field(0),
        field(2).and_then(|field| field.parse::<i32>().ok()),
        field(3).and_then(|field| field.parse::<i32>().ok()),
    ) else {
        return Err(format!("Process {pid} has an unreadable group identity"));
    };
    Ok(Some(Stat {
        zombie: state == "Z" || state == "X",
        group,
        session,
        // A test's synthetic entry may stop after the session.
        start: field(19)
            .and_then(|field| field.parse().ok())
            .unwrap_or_default(),
    }))
}

/// Whether the ID `original.pid` names still belongs to `original`: it is
/// still that process, or no process has that PID, so nothing has reused it.
fn id_kept(proc: &Path, original: &Process) -> Result<bool, String> {
    Ok(match read_stat(&proc.join(original.pid.to_string()))? {
        None => true,
        Some(stat) => stat.start == original.start,
    })
}

/// Every process in `helper`'s session or `provider`'s group, proved or not.
pub(crate) fn census(helper: &Process, provider: &Process) -> Result<Vec<Survivor>, String> {
    census_in(Path::new("/proc"), helper, provider)
}

pub(crate) fn census_in(
    proc: &Path,
    helper: &Process,
    provider: &Process,
) -> Result<Vec<Survivor>, String> {
    let session_kept = id_kept(proc, helper)?;
    let group_kept = id_kept(proc, provider)?;
    let uid = crate::daemon::current_uid();
    scan(proc, |process, pid, stat| {
        // A reused ID's session or group is its new owner's.
        let in_session = session_kept && stat.session == helper.pid;
        let in_group = group_kept && stat.group == provider.pid;
        if !in_session && !in_group {
            return Ok(None);
        }
        let proved = owned_by(process, uid)?
            && ((in_session && stat.start >= helper.start)
                || (in_group && stat.start >= provider.start));
        Ok(Some(survivor(process, pid, stat, proved)))
    })
}

/// Every process proved to be `pane`'s Codex provider because its arguments
/// name the conversation's own listener directory, and the members of each
/// one's process group when it leads it. `named` lists those providers.
pub(crate) fn listener_census(named: &[Process]) -> Result<Vec<Survivor>, String> {
    let proc = Path::new("/proc");
    let uid = crate::daemon::current_uid();
    scan(proc, |process, pid, stat| {
        let provider = named
            .iter()
            .find(|named| named.pid == pid || named.pid == stat.group);
        let Some(provider) = provider else {
            return Ok(None);
        };
        let proved = owned_by(process, uid)?
            && stat.start >= provider.start
            && (pid != provider.pid || stat.start == provider.start)
            && id_kept(proc, provider)?;
        Ok(Some(survivor(process, pid, stat, proved)))
    })
}

/// Whether `process` runs as `uid`, by its real user ID, which is what
/// permits signalling it. Its `/proc` directory's owner is no proof: a
/// process that made itself undumpable, as ssh does, shows root there.
fn owned_by(process: &Path, uid: u32) -> Result<bool, String> {
    let status = match std::fs::read_to_string(process.join("status")) {
        Ok(status) => status,
        // Gone since it was listed: nothing to prove or end.
        Err(error)
            if error.kind() == io::ErrorKind::NotFound
                || error.raw_os_error() == Some(libc::ESRCH) =>
        {
            return Ok(false);
        }
        Err(error) => {
            return Err(format!(
                "Cannot read process {}: {error}",
                process.display()
            ));
        }
    };
    Ok(status
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))
        .and_then(|ids| ids.split_ascii_whitespace().next())
        .and_then(|real| real.parse::<u32>().ok())
        == Some(uid))
}

fn scan(
    proc: &Path,
    mut member: impl FnMut(&Path, i32, Stat) -> Result<Option<Survivor>, String>,
) -> Result<Vec<Survivor>, String> {
    let entries = std::fs::read_dir(proc)
        .map_err(|error| format!("Cannot enumerate processes for exact absence: {error}"))?;
    let mut found = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| format!("Cannot enumerate processes: {error}"))?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<i32>().ok())
        else {
            continue;
        };
        let process = entry.path();
        let Some(stat) = read_stat(&process)? else {
            continue;
        };
        if let Some(survivor) = member(&process, pid, stat)? {
            found.push(survivor);
        }
    }
    found.sort_by_key(|survivor| survivor.pid);
    Ok(found)
}

/// How much of a command line a person is shown.
const COMMAND_CHARS: usize = 160;

fn survivor(process: &Path, pid: i32, stat: Stat, proved: bool) -> Survivor {
    let name = std::fs::read_to_string(process.join("comm"))
        .map(|name| name.trim_end().to_owned())
        .unwrap_or_default();
    let command = std::fs::read(process.join("cmdline"))
        .map(|argv| {
            let argv = argv.strip_suffix(b"\0").unwrap_or(&argv);
            let line = String::from_utf8_lossy(argv).replace('\0', " ");
            match line.char_indices().nth(COMMAND_CHARS) {
                Some((end, _)) => format!("{}\u{2026}", &line[..end]),
                None => line,
            }
        })
        .unwrap_or_default();
    Survivor {
        pid,
        start: stat.start,
        group: stat.group,
        session: stat.session,
        zombie: stat.zombie,
        name,
        command,
        proved,
    }
}

/// How long a process asked to stop gets before it is killed.
const TERMINATION_GRACE: Duration = Duration::from_secs(2);
/// How long ending waits, in all, for what it ended to be collected.
const ENDING_LIMIT: Duration = Duration::from_secs(10);

/// Ends each survivor `scan` finds that is proved and `chosen`, by its PID
/// and start time, then returns everything still left.
///
/// Each is asked to stop with SIGTERM, then killed if it is still running
/// after a short grace. The scan is repeated until none of them is left or
/// the limit passes. Unproved survivors, and any not chosen, are never
/// signalled; they stay in what is returned, for the person to see.
pub(crate) fn end(
    mut scan: impl FnMut() -> Result<Vec<Survivor>, String>,
    chosen: &[(i32, u64)],
) -> Result<Vec<Survivor>, String> {
    let deadline = Instant::now() + ENDING_LIMIT;
    let ends =
        |survivor: &&Survivor| survivor.proved && chosen.contains(&(survivor.pid, survivor.start));
    loop {
        let found = scan()?;
        let running: Vec<&Survivor> = found
            .iter()
            .filter(ends)
            .filter(|survivor| !survivor.zombie)
            .collect();
        let collecting = found.iter().filter(ends).any(|survivor| survivor.zombie);
        if (running.is_empty() && !collecting) || Instant::now() >= deadline {
            return Ok(found);
        }
        if running.is_empty() {
            // Its parent, or init once that has gone, collects it shortly.
            std::thread::sleep(Duration::from_millis(20));
            continue;
        }
        let mut pinned = Vec::with_capacity(running.len());
        for survivor in running {
            if let Some(process) = Pinned::open(survivor)? {
                pinned.push(process);
            }
        }
        if pinned.is_empty() {
            // Each exited or changed between the scan and the pidfd; the next
            // scan says which, without spinning on /proc meanwhile.
            std::thread::sleep(Duration::from_millis(20));
            continue;
        }
        for process in &pinned {
            process.signal(libc::SIGTERM)?;
        }
        let left = wait_for_exit(pinned, TERMINATION_GRACE)?;
        for process in &left {
            process.signal(libc::SIGKILL)?;
        }
        wait_for_exit(left, TERMINATION_GRACE)?;
    }
}

/// A survivor held by a pidfd, so that signalling it can never reach another
/// process that later takes its PID.
struct Pinned {
    pidfd: File,
    pid: i32,
}

impl Pinned {
    /// Pins `survivor` if it is still the process the scan found, in the same
    /// group and session. `None` once it has gone or changed.
    fn open(survivor: &Survivor) -> Result<Option<Self>, String> {
        // SAFETY: scalar arguments; success returns a fresh owned descriptor.
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, survivor.pid, 0) };
        if raw < 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ESRCH) {
                return Ok(None);
            }
            return Err(format!("Cannot hold process {}: {error}", survivor.pid));
        }
        // SAFETY: successful pidfd_open above yielded a fresh owned descriptor.
        let pidfd = unsafe { File::from_raw_fd(raw as libc::c_int) };
        // Read after pinning: only the same process can still show the same
        // start time under this PID, so the descriptor names the survivor.
        let same =
            read_stat(&Path::new("/proc").join(survivor.pid.to_string()))?.is_some_and(|stat| {
                stat.start == survivor.start
                    && stat.group == survivor.group
                    && stat.session == survivor.session
                    && !stat.zombie
            });
        Ok(same.then_some(Self {
            pidfd,
            pid: survivor.pid,
        }))
    }

    fn signal(&self, signal: i32) -> Result<(), String> {
        // SAFETY: the descriptor is a live pidfd owned by self; no siginfo.
        let sent = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                self.pidfd.as_raw_fd(),
                signal,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            )
        };
        if sent < 0 {
            let error = io::Error::last_os_error();
            // It has already exited.
            if error.raw_os_error() != Some(libc::ESRCH) {
                return Err(format!("Cannot end process {}: {error}", self.pid));
            }
        }
        Ok(())
    }
}

/// Waits up to `limit` for every pinned process to exit, and returns those
/// still running.
fn wait_for_exit(mut pinned: Vec<Pinned>, limit: Duration) -> Result<Vec<Pinned>, String> {
    let deadline = Instant::now() + limit;
    while !pinned.is_empty() {
        let mut polled: Vec<libc::pollfd> = pinned
            .iter()
            .map(|process| libc::pollfd {
                fd: process.pidfd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            })
            .collect();
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        // SAFETY: `polled` holds initialized pollfds for descriptors that
        // `pinned` keeps open for the duration of the call.
        let ready = unsafe {
            libc::poll(
                polled.as_mut_ptr(),
                polled.len() as libc::nfds_t,
                remaining.as_millis().min(i32::MAX as u128) as i32,
            )
        };
        if ready < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(format!("Cannot wait for ended processes: {error}"));
        }
        let mut index = 0;
        pinned.retain(|_| {
            let exited = polled[index].revents != 0;
            index += 1;
            !exited
        });
    }
    Ok(pinned)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stat(root: &Path, pid: i32, group: i32, session: i32, start: u64) {
        stat_as(
            root,
            pid,
            group,
            session,
            start,
            crate::daemon::current_uid(),
        );
    }

    fn stat_as(root: &Path, pid: i32, group: i32, session: i32, start: u64, uid: u32) {
        let directory = root.join(pid.to_string());
        std::fs::create_dir_all(&directory).unwrap();
        // Fields after the name: state ppid pgrp session, then 15 more before
        // the start time.
        let filler = vec!["0"; 15].join(" ");
        std::fs::write(
            directory.join("stat"),
            format!("{pid} (odd ) name) S 1 {group} {session} {filler} {start}"),
        )
        .unwrap();
        std::fs::write(directory.join("comm"), "odd ) name\n").unwrap();
        std::fs::write(directory.join("cmdline"), b"tool\0--flag\0").unwrap();
        std::fs::write(
            directory.join("status"),
            format!("Name:\todd\nUid:\t{uid}\t{uid}\t{uid}\t{uid}\n"),
        )
        .unwrap();
    }

    #[test]
    fn membership_proves_a_survivor_only_while_its_ids_are_unreused() {
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let helper = Process {
            pid: 100,
            start: 50,
        };
        let provider = Process {
            pid: 101,
            start: 60,
        };
        // Helper and provider gone; a tool left in the provider's group, and
        // one left in the helper's session.
        stat(root.path(), 200, 101, 100, 70);
        stat(root.path(), 201, 201, 100, 80);
        // Unrelated.
        stat(root.path(), 300, 300, 300, 10);
        let found = census_in(root.path(), &helper, &provider).unwrap();
        assert_eq!(
            found.iter().map(|s| (s.pid, s.proved)).collect::<Vec<_>>(),
            [(200, true), (201, true)]
        );
        assert_eq!(found[0].name, "odd ) name");
        assert_eq!(found[0].command, "tool --flag");

        // Something that started before the provider can't be its child.
        stat(root.path(), 202, 101, 202, 55);
        let found = census_in(root.path(), &helper, &provider).unwrap();
        assert!(!found.iter().find(|s| s.pid == 202).unwrap().proved);

        // Another user's process is never this user's to end.
        stat_as(
            root.path(),
            203,
            101,
            100,
            75,
            crate::daemon::current_uid() + 1,
        );
        let found = census_in(root.path(), &helper, &provider).unwrap();
        assert!(!found.iter().find(|s| s.pid == 203).unwrap().proved);

        // Another process now has the helper's PID, so the helper's session
        // emptied before it was reused: what is in a session of that ID now
        // is the new owner's, and neither holds nor is ended.
        stat(root.path(), 100, 100, 100, 90);
        let found = census_in(root.path(), &helper, &provider).unwrap();
        assert_eq!(
            found.iter().map(|s| (s.pid, s.proved)).collect::<Vec<_>>(),
            [(200, true), (202, false), (203, false)]
        );

        // The same for the provider's group.
        stat(root.path(), 101, 101, 101, 95);
        assert_eq!(census_in(root.path(), &helper, &provider).unwrap(), []);
    }

    #[test]
    fn ending_signals_only_chosen_proved_survivors_and_reports_what_is_left() {
        use std::process::Command;
        let mut proved = Command::new("/bin/sleep").arg("1000").spawn().unwrap();
        let mut spared = Command::new("/bin/sleep").arg("1000").spawn().unwrap();
        let mut unseen = Command::new("/bin/sleep").arg("1000").spawn().unwrap();
        let identity = |child: &std::process::Child| Process::read(child.id() as i32).unwrap();
        let (proved_identity, spared_identity, unseen_identity) =
            (identity(&proved), identity(&spared), identity(&unseen));
        let survivor = |process: &Process, proved: bool| {
            let stat = read_stat(&Path::new("/proc").join(process.pid.to_string()))
                .unwrap()
                .unwrap();
            Survivor {
                pid: process.pid,
                start: stat.start,
                group: stat.group,
                session: stat.session,
                zombie: stat.zombie,
                name: String::new(),
                command: String::new(),
                proved,
            }
        };
        let scan = || -> Result<Vec<Survivor>, String> {
            Ok([
                Process::read(proved_identity.pid)
                    .ok()
                    .filter(|process| *process == proved_identity)
                    .map(|process| survivor(&process, true)),
                Some(survivor(&spared_identity, false)),
                // Proved, but found after the person chose what to end.
                Some(survivor(&unseen_identity, true)),
            ]
            .into_iter()
            .flatten()
            .filter(|survivor| !survivor.zombie)
            .collect())
        };
        let chosen = [&proved_identity, &spared_identity]
            .map(|process| (process.pid, survivor(process, true).start));
        let mut left = end(scan, &chosen)
            .unwrap()
            .iter()
            .map(|s| s.pid)
            .collect::<Vec<_>>();
        left.sort();
        let mut expected = vec![spared_identity.pid, unseen_identity.pid];
        expected.sort();
        assert_eq!(left, expected);
        assert!(proved.wait().unwrap().code().is_none());
        assert!(
            spared.try_wait().unwrap().is_none(),
            "an unproved survivor was signalled"
        );
        assert!(
            unseen.try_wait().unwrap().is_none(),
            "a survivor nobody chose was signalled"
        );
        for mut child in [spared, unseen] {
            child.kill().unwrap();
            child.wait().unwrap();
        }
    }

    #[test]
    fn a_survivor_whose_pid_changed_hands_is_not_signalled() {
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("1000")
            .spawn()
            .unwrap();
        let identity = Process::read(child.id() as i32).unwrap();
        let stat = read_stat(&Path::new("/proc").join(identity.pid.to_string()))
            .unwrap()
            .unwrap();
        let stale = Survivor {
            pid: identity.pid,
            start: stat.start + 1,
            group: stat.group,
            session: stat.session,
            zombie: false,
            name: String::new(),
            command: String::new(),
            proved: true,
        };
        assert!(Pinned::open(&stale).unwrap().is_none());
        assert!(child.try_wait().unwrap().is_none());
        child.kill().unwrap();
        child.wait().unwrap();
    }
}
