//! The daemon's open-descriptor limit: raised for the daemon, never for the
//! processes it starts.
//!
//! Each terminal costs the daemon about fifteen descriptors - its PTY, its
//! holder's socket and pidfds, and the pipes that wake its threads - so the
//! customary soft limit of 1024 would cap a daemon at a few dozen terminals.
//! Only the daemon needs the room. Programs that still use `select` break on
//! descriptors past 1024, and some size tables or close loops by the limit,
//! so every child starts with the limit the daemon itself was given.

use std::{
    io,
    os::unix::process::CommandExt,
    process::Command,
    sync::{
        OnceLock,
        atomic::{AtomicI32, Ordering},
    },
};

/// The limit this daemon was started with, recorded only if it was raised.
static INHERITED: OnceLock<libc::rlimit> = OnceLock::new();

/// The soft limit this process raised itself to, or 0 if it never raised it.
/// An atomic, so that a child can read it between fork and exec.
static RAISED: AtomicI32 = AtomicI32::new(0);

/// The highest soft limit this process has run with, so no descriptor it
/// holds is numbered at or above it, even after a pre-exec hook lowered the
/// limit for a child. Async-signal-safe.
pub fn highest() -> libc::c_int {
    RAISED.load(Ordering::Relaxed)
}

/// Raises this process's soft open-descriptor limit to its hard limit.
pub fn raise() {
    let Some(limit) = current() else {
        return;
    };
    let ceiling = ceiling(limit.rlim_max);
    if limit.rlim_cur >= ceiling {
        return;
    }
    let inherited = *INHERITED.get_or_init(|| limit);
    let raised = libc::rlimit {
        rlim_cur: ceiling,
        rlim_max: limit.rlim_max,
    };
    // SAFETY: `raised` is a live rlimit, and a soft limit equal to the hard
    // one is always permitted.
    if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &raised) } == -1 {
        tracing::warn!(
            error = %io::Error::last_os_error(),
            limit = inherited.rlim_cur,
            "cannot raise the open-descriptor limit; the daemon holds fewer terminals"
        );
        return;
    }
    RAISED.store(
        libc::c_int::try_from(ceiling).unwrap_or(libc::c_int::MAX),
        Ordering::Relaxed,
    );
}

/// The highest soft limit the kernel accepts under `hard`. Linux accepts the
/// hard limit itself.
#[cfg(target_os = "linux")]
fn ceiling(hard: libc::rlim_t) -> libc::rlim_t {
    hard
}

/// Darwin reports an unlimited hard limit but refuses a soft limit above
/// `kern.maxfilesperproc`, so the raise stops there.
#[cfg(target_os = "macos")]
fn ceiling(hard: libc::rlim_t) -> libc::rlim_t {
    let mut per_process: libc::c_int = 0;
    let mut size = std::mem::size_of_val(&per_process);
    let mut name = [libc::CTL_KERN, libc::KERN_MAXFILESPERPROC];
    // SAFETY: the name and the output buffer, with its size, are live for the
    // call, which only writes the buffer and its size.
    let read = unsafe {
        libc::sysctl(
            name.as_mut_ptr(),
            2,
            (&raw mut per_process).cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    } == 0;
    match libc::rlim_t::try_from(per_process) {
        Ok(per_process) if read && per_process > 0 => hard.min(per_process),
        // `OPEN_MAX` from <sys/syslimits.h>, which the kernel always accepts.
        _ => hard.min(10_240),
    }
}

/// Starts `command` with the limit this daemon was started with.
pub fn inherit(command: &mut Command) {
    let Some(&limit) = INHERITED.get() else {
        return;
    };
    // SAFETY: setrlimit is async-signal-safe, and lowering a soft limit is
    // always permitted.
    unsafe {
        command.pre_exec(move || {
            if libc::setrlimit(libc::RLIMIT_NOFILE, &limit) == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

fn current() -> Option<libc::rlimit> {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `limit` is a live rlimit for the call to fill in.
    (unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } == 0).then_some(limit)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Call only in a private subprocess: both the limit and INHERITED belong
    /// to the process, rather than to the current test thread.
    pub(crate) fn raise_with_fixture_limit() -> libc::rlimit {
        let mut limit = current().unwrap();
        assert!(
            limit.rlim_max > 64,
            "fixture needs room below the hard limit"
        );
        limit.rlim_cur = limit.rlim_cur.min(1024).min(limit.rlim_max - 1);
        // SAFETY: the initialized value only lowers the current soft limit.
        assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) }, 0);
        raise();
        assert_daemon_limit_remains_raised(&limit);
        limit
    }

    pub(crate) fn assert_daemon_limit_remains_raised(original: &libc::rlimit) {
        let raised = current().unwrap();
        assert_eq!(raised.rlim_cur, ceiling(original.rlim_max));
        assert_eq!(libc::rlim_t::try_from(highest()).unwrap(), raised.rlim_cur);
        assert_eq!(raised.rlim_max, original.rlim_max);
    }

    pub(crate) fn assert_process_limit(pid: u32, expected: &libc::rlimit) {
        let limits = std::fs::read_to_string(format!("/proc/{pid}/limits")).unwrap();
        let mut fields = limits
            .lines()
            .find_map(|line| line.strip_prefix("Max open files"))
            .expect("kernel reports the open-descriptor limit")
            .split_whitespace();
        let format = |limit: libc::rlim_t| {
            if limit == libc::RLIM_INFINITY {
                "unlimited".to_owned()
            } else {
                limit.to_string()
            }
        };
        assert_eq!(
            fields.next().unwrap(),
            format(expected.rlim_cur),
            "process {pid} must retain the incoming soft descriptor limit"
        );
        assert_eq!(
            fields.next().unwrap(),
            format(expected.rlim_max),
            "process {pid} must retain the incoming hard descriptor limit"
        );
    }

    #[test]
    fn children_start_with_the_limit_the_daemon_was_given() {
        const CHILD: &str = "CAGENTS_TEST_DESCRIPTOR_LIMIT";
        if std::env::var_os(CHILD).is_none() {
            let before = current().unwrap();
            let output = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "descriptor_limit::tests::children_start_with_the_limit_the_daemon_was_given",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "isolated descriptor-limit test failed: {}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            let after = current().unwrap();
            assert_eq!(after.rlim_cur, before.rlim_cur);
            assert_eq!(after.rlim_max, before.rlim_max);
            return;
        }
        // Both RLIMIT_NOFILE and INHERITED belong to the process. Exercise
        // them in isolation from the descriptor-heavy parallel host tests.
        // A daemon started with the customary soft limit, below its hard one.
        let limit = raise_with_fixture_limit();

        let mut child = Command::new("/bin/sh");
        child.args(["-c", "ulimit -n"]);
        inherit(&mut child);
        let output = child.output().unwrap();
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            INHERITED.get().unwrap().rlim_cur.to_string()
        );
        assert_daemon_limit_remains_raised(&limit);
    }
}
