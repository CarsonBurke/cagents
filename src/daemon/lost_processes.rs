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
//! The Linux census follows the membership rules above. Darwin census
//! requires the recorded leader still to have its exact start time before
//! authorizing signals. A reaped leader leaves a numeric ID whose generation
//! cannot be checked, and a vanished session does not prove detached work
//! never left that session.
//!
//! Ending signals only the processes the person was shown and chose to end,
//! each as it was when shown: its PID and its start time. It pins each with
//! a pidfd and reads it again before signalling it, so a PID that changed
//! hands since fails that reading, and a pidfd can never signal another
//! process on Linux. Darwin opens the recorded epoch with a kqueue pin and
//! has the check-to-signal interval documented by `PinnedProcess`. A process
//! found since is shown before anything ends it.
use crate::{daemon::holder_registry::Process, pinned_process::PinnedProcess};
use std::{
    io,
    os::fd::AsRawFd,
    time::{Duration, Instant},
};

/// A process left in a lost helper's session or its provider's group.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Survivor {
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

#[cfg(target_os = "macos")]
#[path = "lost_processes/darwin.rs"]
mod platform;
#[cfg(target_os = "linux")]
#[path = "lost_processes/linux.rs"]
mod platform;

#[cfg(target_os = "macos")]
pub use platform::listener_providers;
pub use platform::{census, listener_census};

/// Trim the command line to what can be shown in the close dialog.
fn command_for_display(argv: &[u8]) -> String {
    const COMMAND_CHARS: usize = 160;
    let argv = argv.strip_suffix(b"\0").unwrap_or(argv);
    let line = String::from_utf8_lossy(argv).replace('\0', " ");
    match line.char_indices().nth(COMMAND_CHARS) {
        Some((end, _)) => format!("{}\u{2026}", &line[..end]),
        None => line,
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
pub fn end(
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
            // Each exited or changed between the scan and the pin; the next
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

/// A survivor held by a pin for its recorded start time.
struct Pinned {
    process: PinnedProcess,
    pid: i32,
}

impl Pinned {
    /// Pins `survivor` if it is still the process the scan found, in the same
    /// group and session. `None` once it has gone or changed.
    fn open(survivor: &Survivor) -> Result<Option<Self>, String> {
        Ok(platform::pin_matching(survivor)?.map(|process| Self {
            process,
            pid: survivor.pid,
        }))
    }

    fn signal(&self, signal: i32) -> Result<(), String> {
        // `Ok(false)`: it has already exited.
        self.process
            .signal(signal)
            .map(drop)
            .map_err(|error| format!("Cannot end process {}: {error}", self.pid))
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
                fd: process.process.exit_readiness().as_raw_fd(),
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
        let mut remaining = Vec::with_capacity(pinned.len());
        for (process, polled) in pinned.into_iter().zip(&polled) {
            // Only a readiness that reports an exit removes a process; an
            // invalid or failed descriptor proves nothing.
            if polled.revents == 0
                || !process
                    .process
                    .exited()
                    .map_err(|error| format!("Cannot wait for ended processes: {error}"))?
            {
                remaining.push(process);
            }
        }
        pinned = remaining;
    }
    Ok(pinned)
}
