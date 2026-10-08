//! The daemon's open-descriptor limit: raised for the daemon, never for the
//! processes it starts.
//!
//! Each terminal costs the daemon about fifteen descriptors - its PTY, its
//! holder's socket and pidfds, and the pipes that wake its threads - so the
//! customary soft limit of 1024 would cap a daemon at a few dozen terminals.
//! Only the daemon needs the room. Programs that still use `select` break on
//! descriptors past 1024, and some size tables or close loops by the limit,
//! so every child starts with the limit the daemon itself was given.

use std::{io, os::unix::process::CommandExt, process::Command, sync::OnceLock};

/// The limit this daemon was started with, recorded only if it was raised.
static INHERITED: OnceLock<libc::rlimit> = OnceLock::new();

/// Raises this process's soft open-descriptor limit to its hard limit.
pub(crate) fn raise() {
    let Some(limit) = current() else {
        return;
    };
    if limit.rlim_cur >= limit.rlim_max {
        return;
    }
    let inherited = *INHERITED.get_or_init(|| limit);
    let raised = libc::rlimit {
        rlim_cur: limit.rlim_max,
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
    }
}

/// Starts `command` with the limit this daemon was started with.
pub(crate) fn inherit(command: &mut Command) {
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
mod tests {
    use super::*;

    #[test]
    fn children_start_with_the_limit_the_daemon_was_given() {
        const CHILD: &str = "CAGENTS_TEST_DESCRIPTOR_LIMIT";
        if std::env::var_os(CHILD).is_none() {
            let before = current().unwrap();
            let output = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "host::descriptor_limit::tests::children_start_with_the_limit_the_daemon_was_given",
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
        let mut limit = current().unwrap();
        // A daemon started with the customary soft limit, below its hard one.
        limit.rlim_cur = limit.rlim_cur.min(1024).min(limit.rlim_max - 1);
        // SAFETY: `limit` is a live rlimit; lowering a soft limit is always
        // permitted.
        assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) }, 0);
        raise();
        let raised = current().unwrap();
        assert_eq!(raised.rlim_cur, raised.rlim_max);

        let mut child = Command::new("/bin/sh");
        child.args(["-c", "ulimit -n"]);
        inherit(&mut child);
        let output = child.output().unwrap();
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            INHERITED.get().unwrap().rlim_cur.to_string()
        );
    }
}
