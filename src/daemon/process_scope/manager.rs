//! Requests to the OS user manager after enrollment: settling a scope request
//! that was given up on, and collecting session scopes left empty.
//!
//! A scope systemd starts for a process that is already exiting holds
//! nothing: the kernel skips migrating an exiting task, yet systemd reports
//! the scope started. An empty scope never becomes empty again, so systemd
//! never stops it, and it stays loaded until the user manager exits.
use std::{
    io::{self, Read},
    os::unix::process::CommandExt,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use serde_json::Value;

use super::{POLL, Refused, manager_runtime, workload::cgroup_path};

/// Bounds each per-unit call of a collection, and settling a request given
/// up on.
const CALL_TIMEOUT: Duration = Duration::from_secs(5);
/// Bounds a collection's listing of every session scope, which a manager
/// with thousands loaded answers slowly.
const LISTING_TIMEOUT: Duration = Duration::from_secs(30);
pub(super) const SCOPES: &str = "cagents-session-*.scope";

/// One `busctl --user` call through the bus broker, the route a scope
/// request takes, ended at `deadline`. A systemd error reply is [`Refused`].
fn busctl(arguments: &[&str], deadline: Instant) -> io::Result<Vec<Value>> {
    let mut command = Command::new("busctl");
    command
        .env("XDG_RUNTIME_DIR", manager_runtime())
        .env_remove("DBUS_SESSION_BUS_ADDRESS")
        // The deadline bounds the call; busctl's own must not end it first.
        .args(["--user", "--json=short", "--timeout=60s"])
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    // SAFETY: only the existing close_range syscall runs after fork.
    unsafe {
        command.pre_exec(crate::daemon::seal_inherited_descriptors);
    }
    // Held until the final wait, so a retained actor's orphan sweep leaves
    // this status to us.
    let (mut child, _registration) = crate::daemon::adopted_orphans::spawn_owned(&mut command)?;
    let mut stdout = child.stdout.take().expect("busctl stdout is piped");
    // A unit listing can outgrow the pipe: read it while busctl runs.
    let reader = thread::spawn(move || {
        let mut output = String::new();
        stdout.read_to_string(&mut output).map(|_| output)
    });
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            let _ = reader.join();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "user manager call timed out",
            ));
        }
        thread::sleep(POLL);
    };
    let output = reader
        .join()
        .map_err(|_| io::Error::other("busctl output reader panicked"))??;
    if !status.success() {
        return Err(io::Error::other(Refused(status)));
    }
    output
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).map_err(io::Error::other))
        .collect()
}

fn refused(error: &io::Error) -> bool {
    error.get_ref().is_some_and(|inner| inner.is::<Refused>())
}

fn manager(
    method: &str,
    signature: &str,
    arguments: &[&str],
    deadline: Instant,
) -> io::Result<Vec<Value>> {
    let mut call = vec![
        "call",
        "org.freedesktop.systemd1",
        "/org/freedesktop/systemd1",
        "org.freedesktop.systemd1.Manager",
        method,
        signature,
    ];
    call.extend_from_slice(arguments);
    busctl(&call, deadline)
}

fn malformed() -> io::Error {
    io::Error::other("unexpected user manager reply")
}

/// The unit's object path, or `None` when systemd has no such unit loaded.
fn unit_path(unit: &str, deadline: Instant) -> io::Result<Option<String>> {
    match manager("GetUnit", "s", &[unit], deadline) {
        Ok(reply) => reply
            .first()
            .and_then(|reply| reply["data"][0].as_str())
            .map(|path| Some(path.to_owned()))
            .ok_or_else(malformed),
        // Only a scope request can load a session scope's unit.
        Err(error) if refused(&error) => Ok(None),
        Err(error) => Err(error),
    }
}

/// The state a decision about a session scope rests on.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Scope {
    pub(super) active_state: String,
    /// The unit's queued job, or 0.
    pub(super) job: u64,
    /// When the unit last became active, in CLOCK_MONOTONIC microseconds.
    pub(super) active_since: u64,
    pub(super) control_group: String,
}

fn scope(path: &str, deadline: Instant) -> io::Result<Scope> {
    let unit = busctl(
        &[
            "get-property",
            "org.freedesktop.systemd1",
            path,
            "org.freedesktop.systemd1.Unit",
            "ActiveState",
            "Job",
            "ActiveEnterTimestampMonotonic",
        ],
        deadline,
    )?;
    let scope = busctl(
        &[
            "get-property",
            "org.freedesktop.systemd1",
            path,
            "org.freedesktop.systemd1.Scope",
            "ControlGroup",
        ],
        deadline,
    )?;
    let [active_state, job, active_since] = unit.as_slice() else {
        return Err(malformed());
    };
    Ok(Scope {
        active_state: active_state["data"]
            .as_str()
            .ok_or_else(malformed)?
            .to_owned(),
        job: job["data"][0].as_u64().ok_or_else(malformed)?,
        active_since: active_since["data"].as_u64().ok_or_else(malformed)?,
        control_group: scope
            .first()
            .and_then(|group| group["data"].as_str())
            .ok_or_else(malformed)?
            .to_owned(),
    })
}

/// Whether a cgroup or any cgroup below it holds a process.
fn populated(control_group: &str) -> io::Result<bool> {
    let events = std::fs::read_to_string(cgroup_path(control_group)?.join("cgroup.events"))?;
    events
        .lines()
        .find_map(|line| line.strip_prefix("populated "))
        .map(|populated| populated != "0")
        .ok_or_else(malformed)
}

/// What to do about a session scope.
#[derive(Debug, PartialEq)]
pub(super) enum Settle {
    /// The request still has a job queued: cancel it.
    Cancel(u64),
    /// Starting or stopping: look again shortly.
    Wait,
    /// Running and empty: it can never hold a process.
    Stop,
    /// Holds a process, or is gone: leave it to systemd.
    Leave,
}

/// A running scope is only judged empty once its start job is done, so a
/// process systemd is about to attach is never cut off.
pub(super) fn settle(
    scope: &Scope,
    populated: impl FnOnce() -> io::Result<bool>,
) -> io::Result<Settle> {
    if scope.job != 0 {
        return Ok(Settle::Cancel(scope.job));
    }
    Ok(match scope.active_state.as_str() {
        "active" if !populated()? => Settle::Stop,
        "active" => Settle::Leave,
        "activating" | "deactivating" | "reloading" | "refreshing" => Settle::Wait,
        _ => Settle::Leave,
    })
}

/// Settle a scope request given up on before its membership was confirmed,
/// which systemd may still have queued. Returns the membership when systemd
/// placed the process after all.
///
/// A unit that isn't loaded yet is taken as never started. These calls go on
/// connections of their own, which D-Bus doesn't order after the request's,
/// though the broker delivers them that way in practice. Should one overtake
/// it, the scope it starts is only left empty, and collection stops it.
pub(super) fn withdraw(
    unit: &str,
    membership: impl Fn() -> io::Result<String>,
) -> io::Result<Option<String>> {
    let deadline = Instant::now() + CALL_TIMEOUT;
    while Instant::now() < deadline {
        let Some(path) = unit_path(unit, deadline)? else {
            return Ok(None);
        };
        let current = scope(&path, deadline)?;
        // A process systemd placed late is enrolled, whatever is empty.
        if current.job == 0
            && let Ok(membership) = membership()
            && membership.rsplit('/').next() == Some(unit)
        {
            return Ok(Some(membership));
        }
        match settle(&current, || populated(&current.control_group))? {
            Settle::Cancel(job) => {
                // A job that has just run can't be cancelled; look again.
                let job = job.to_string();
                match manager("CancelJob", "u", &[&job], deadline) {
                    Ok(_) => {}
                    Err(error) if refused(&error) => {}
                    Err(error) => return Err(error),
                }
            }
            Settle::Wait => thread::sleep(POLL),
            Settle::Stop => {
                stop(unit, deadline)?;
                return Ok(None);
            }
            Settle::Leave => return Ok(None),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        "the scope request could not be withdrawn",
    ))
}

/// Stop a scope only if no job is queued for it: a stop never replaces a
/// start that would attach a process.
fn stop(unit: &str, deadline: Instant) -> io::Result<()> {
    manager("StopUnit", "ss", &[unit, "fail"], deadline).map(drop)
}

fn monotonic_now() -> Duration {
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: clock_gettime writes the initialized timespec it is given.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now) };
    Duration::new(now.tv_sec as u64, now.tv_nsec as u32)
}

/// Whether a session scope has been running for at least `age`.
pub(super) fn running_for(scope: &Scope, age: Duration, now: Duration) -> bool {
    now.saturating_sub(Duration::from_micros(scope.active_since)) >= age
}

/// Stop the session scopes that have run for at least `age` and hold no
/// process, such as those an enrollment couldn't withdraw. Returns how many
/// were stopped. A scope with a process or a queued job is never stopped.
pub(super) fn collect_empty_scopes(age: Duration) -> io::Result<usize> {
    let call = || Instant::now() + CALL_TIMEOUT;
    // Thousands of leaked units make a long listing from a busy manager.
    let listing = manager(
        "ListUnitsByPatterns",
        "asas",
        &["1", "active", "1", SCOPES],
        Instant::now() + LISTING_TIMEOUT,
    )?;
    let units = listing
        .first()
        .and_then(|reply| reply["data"][0].as_array())
        .ok_or_else(malformed)?;
    let mut stopped = 0;
    for unit in units {
        let (Some(name), Some(path), Some(0)) =
            (unit[0].as_str(), unit[6].as_str(), unit[7].as_u64())
        else {
            continue;
        };
        let Ok(current) = scope(path, call()) else {
            continue;
        };
        if !running_for(&current, age, monotonic_now()) {
            continue;
        }
        // A scope that stops meanwhile takes its cgroup with it: skip it,
        // as anything this pass can't judge.
        if settle(&current, || populated(&current.control_group))
            .is_ok_and(|settle| settle == Settle::Stop)
            && stop(name, call()).is_ok()
        {
            stopped += 1;
        }
    }
    Ok(stopped)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope(active_state: &str, job: u64) -> Scope {
        Scope {
            active_state: active_state.into(),
            job,
            active_since: 0,
            control_group: "/app.slice/cagents-session-x.scope".into(),
        }
    }

    #[test]
    fn only_a_running_scope_without_a_job_or_process_is_stopped() {
        let empty = || Ok(false);
        let occupied = || Ok(true);
        assert_eq!(settle(&scope("active", 0), empty).unwrap(), Settle::Stop);
        assert_eq!(
            settle(&scope("active", 0), occupied).unwrap(),
            Settle::Leave
        );
        // A queued start may still attach a process: it is cancelled, not
        // stopped, and emptiness isn't judged.
        assert_eq!(
            settle(&scope("active", 7), || panic!("judged a queued scope")).unwrap(),
            Settle::Cancel(7)
        );
        assert_eq!(
            settle(&scope("inactive", 7), || panic!("judged a queued scope")).unwrap(),
            Settle::Cancel(7)
        );
        assert_eq!(
            settle(&scope("activating", 0), empty).unwrap(),
            Settle::Wait
        );
        assert_eq!(settle(&scope("inactive", 0), empty).unwrap(), Settle::Leave);
        assert_eq!(settle(&scope("failed", 0), empty).unwrap(), Settle::Leave);
        // An unreadable cgroup is not taken for an empty one.
        assert!(settle(&scope("active", 0), || Err(io::Error::other("gone"))).is_err());
    }

    #[test]
    fn a_recently_started_scope_is_not_collected() {
        let mut started = scope("active", 0);
        started.active_since = 10_000_000;
        let minute = Duration::from_secs(60);
        assert!(!running_for(&started, minute, Duration::from_secs(69)));
        assert!(running_for(&started, minute, Duration::from_secs(70)));
        assert!(!running_for(&started, minute, Duration::from_secs(1)));
    }
}
