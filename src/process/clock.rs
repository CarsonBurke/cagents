//! The clocks that date processes and order events within one boot.
//!
//! The kernel dates a process start by a clock of its own. Linux states it
//! in `/proc/<pid>/stat` as `CLOCK_BOOTTIME` ticks of `_SC_CLK_TCK`. Darwin
//! records it as `ri_proc_start_abstime`, in `mach_absolute_time` units,
//! a clock that stops while the machine sleeps. Comparing a reading with a
//! start time is only sound on the clock that dated it, so [`start_ticks_now`]
//! reads that clock. [`since_boot`] is the clock for ordering events: it keeps
//! counting through sleep and no change of the wall clock moves it.

use std::io;

/// The current time on the clock the kernel dates process starts by, in the
/// units process records and lineage launches carry start times in. It is
/// rounded down, so a process forked after this reading never reads as
/// starting earlier.
///
/// # Errors
///
/// Returns the underlying error when the clock cannot be read.
#[cfg(target_os = "linux")]
pub(crate) fn start_ticks_now() -> io::Result<u64> {
    let ticks = u128::from(clock_ticks_per_second()?);
    u64::try_from(since_boot()?.as_nanos() * ticks / 1_000_000_000).map_err(io::Error::other)
}

/// The current time on the clock the kernel dates process starts by, in the
/// units process records and lineage launches carry start times in. A
/// process forked after this reading never reads as starting earlier.
///
/// # Errors
///
/// Never fails on Darwin. The signature matches the Linux reading, which can.
#[cfg(target_os = "macos")]
pub(crate) fn start_ticks_now() -> io::Result<u64> {
    // libc deprecates its binding in favour of the mach2 crate; one function
    // from <mach/mach_time.h> doesn't warrant another dependency.
    unsafe extern "C" {
        safe fn mach_absolute_time() -> u64;
    }
    Ok(mach_absolute_time())
}

/// Time since boot, on a clock that keeps counting while the machine sleeps
/// and that no change of the wall clock moves. Linux names it
/// `CLOCK_BOOTTIME`. Darwin's `CLOCK_MONOTONIC` is the same clock: it reads
/// `mach_continuous_time`.
///
/// # Errors
///
/// Returns the underlying error when the clock cannot be read.
#[cfg(any(target_os = "linux", feature = "ui", test))]
pub(crate) fn since_boot() -> io::Result<std::time::Duration> {
    #[cfg(target_os = "linux")]
    const CLOCK: libc::clockid_t = libc::CLOCK_BOOTTIME;
    #[cfg(target_os = "macos")]
    const CLOCK: libc::clockid_t = libc::CLOCK_MONOTONIC;
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: now is a valid writable timespec for the duration of the call.
    if unsafe { libc::clock_gettime(CLOCK, &mut now) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let seconds = u64::try_from(now.tv_sec).map_err(io::Error::other)?;
    let nanos = u32::try_from(now.tv_nsec)
        .ok()
        .filter(|nanos| *nanos < 1_000_000_000)
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "clock nanoseconds out of range")
        })?;
    Ok(std::time::Duration::new(seconds, nanos))
}

/// When, by the wall clock, the process `pid` began, given the start time
/// the kernel dated it by. Linux keeps no per-process wall-clock start, so
/// the process's age on the boot clock is subtracted from the wall clock now,
/// and a change of the wall clock since the process started skews it. Darwin
/// reads the time the kernel recorded at fork, and answers only while that
/// process still runs as `pid`: a zombie has no record of it.
#[cfg(any(feature = "host", test))]
pub(crate) fn started_at(pid: u32, start: u64) -> Option<std::time::SystemTime> {
    #[cfg(target_os = "linux")]
    {
        let _ = pid;
        let started = std::time::Duration::from_secs_f64(
            start as f64 / f64::from(clock_ticks_per_second().ok()?),
        );
        std::time::SystemTime::now().checked_sub(since_boot().ok()?.checked_sub(started)?)
    }
    #[cfg(target_os = "macos")]
    {
        crate::pinned_process::wall_clock_start_of(pid, start).ok()
    }
}

#[cfg(target_os = "linux")]
fn clock_ticks_per_second() -> io::Result<u32> {
    // SAFETY: sysconf takes a scalar and reads no memory of ours.
    let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    u32::try_from(ticks)
        .ok()
        .filter(|ticks| *ticks > 0)
        .ok_or_else(io::Error::last_os_error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// A child forked between two readings starts between them, by the clock
    /// the kernel dates it with, and its wall-clock start falls in the wall
    /// time those readings span.
    #[test]
    fn a_new_process_starts_between_readings_taken_around_its_spawn() {
        let wall_before = std::time::SystemTime::now();
        let before = start_ticks_now().unwrap();
        let mut child = std::process::Command::new("sleep")
            .arg("5")
            .spawn()
            .unwrap();
        let after = start_ticks_now().unwrap();
        let wall_after = std::time::SystemTime::now();
        let start = started_ticks(child.id());
        let started = started_at(child.id(), start);
        child.kill().unwrap();
        child.wait().unwrap();

        assert!(
            before <= start && start <= after,
            "{before} <= {start} <= {after}"
        );
        let started = started.expect("a running child has a wall-clock start");
        // Linux estimates from ticks of 10ms; allow one tick each side.
        let slack = Duration::from_millis(20);
        assert!(started + slack >= wall_before && started <= wall_after + slack);
    }

    #[test]
    fn since_boot_never_goes_backwards() {
        let first = since_boot().unwrap();
        assert!(since_boot().unwrap() >= first);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_reaped_process_has_no_wall_clock_start() {
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let start = started_ticks(child.id());
        child.wait().unwrap();
        assert_eq!(started_at(child.id(), start), None);
    }

    fn started_ticks(pid: u32) -> u64 {
        #[cfg(target_os = "linux")]
        {
            let stat = std::fs::read(format!("/proc/{pid}/stat")).unwrap();
            crate::model::agent_recovery::process_identity(&stat)
                .unwrap()
                .1
        }
        #[cfg(target_os = "macos")]
        {
            crate::pinned_process::started_ticks_of(pid).unwrap()
        }
    }
}
