//! The `PATH` a daemon adopts from its user's login shell.
//!
//! Whatever starts the daemon hands it its own `PATH`, and on Linux that is
//! often not the one a terminal sees. sshd runs a command, such as a device
//! window's `cagents daemon bridge`, without the login startup files an
//! interactive SSH session reads. A window started by a desktop launcher, or
//! by a session whose login shell is not POSIX, such as fish, carries what the
//! display manager set, which commonly lacks `~/.local/bin` and the other
//! directories installers add in login files. The terminals, provider CLIs and
//! conversations the daemon starts would all miss them, while Accounts, which
//! reads the shell, reports the same CLIs installed. Like the server VS Code
//! starts over SSH, the daemon reads its user's login shell once at startup
//! instead.

use std::{
    collections::HashSet,
    ffi::{OsStr, OsString},
    io::Read,
    os::{fd::AsRawFd, unix::process::CommandExt},
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use super::{
    MAX_SHELL_OUTPUT, SCRUBBED_ENVIRONMENT, SHELL_PATH_COMMAND, SHELL_SCRUBBED_ENVIRONMENT,
    SHELL_TIMEOUT, path_from_output,
};

/// What a login shell is not told, so that startup files guarded on them stay
/// out of the way: an automatic `tmux attach` for SSH logins, or the common
/// `[ "$XDG_VTNR" = 1 ] && exec sway` that starts a compositor from a login on
/// the first virtual terminal, which a daemon started inside that session
/// would otherwise satisfy, its display variables being scrubbed.
const LOGIN_SESSION_ENVIRONMENT: &[&str] = &[
    "SSH_CONNECTION",
    "SSH_CLIENT",
    "SSH_TTY",
    "XDG_VTNR",
    "XDG_SEAT",
    "XDG_SESSION_ID",
    "XDG_SESSION_TYPE",
    "XDG_SESSION_CLASS",
];

/// Set once a daemon has read the login shell, so that what it starts, such as
/// a daemon started in its terminals, does not read it again: the `PATH` they
/// inherit already holds the answer. A launcher whose `PATH` is deliberately
/// complete, such as a test's private daemon, sets it to keep the shell out.
pub const ADOPTED_ENV: &str = "CAGENTS_LOGIN_PATH_ADOPTED";

/// Gives this process the login shell's `PATH`, whichever launcher started
/// it. Directories only the inherited `PATH` names, such as a script's or a
/// test's own, keep their order ahead of it; the rest follow the login's
/// order, so a version manager's shims still precede the system directories a
/// desktop session or sshd hands every process. When the shell does not say,
/// the `PATH` is left as it is. macOS reads the shell for each launch instead.
///
/// # Safety
///
/// Must be called before the process starts a second thread: it changes the
/// process environment, which another thread may be reading.
pub unsafe fn adopt() {
    if !wanted(|name| std::env::var_os(name).is_some()) {
        return;
    }
    // Asked once, answered or not: a shell that failed to answer is not given
    // another five seconds by every daemon this one starts.
    // SAFETY: the caller guarantees the process is still single-threaded.
    unsafe { std::env::set_var(ADOPTED_ENV, "1") };
    let (Ok(shell), Some(home)) = (crate::model::login_shell(), std::env::var_os("HOME")) else {
        return;
    };
    let Some(login) = read(Path::new(&shell), Path::new(&home), SHELL_TIMEOUT) else {
        return;
    };
    if let Some(path) = merged(std::env::var_os("PATH").as_deref(), &login) {
        // SAFETY: the caller guarantees the process is still single-threaded.
        unsafe { std::env::set_var("PATH", path) };
    }
}

/// Whether a process whose environment holds the variables `set` names should
/// read the login shell: any daemon on Linux whose `PATH` no earlier daemon
/// in its line has already answered for.
fn wanted(set: impl Fn(&str) -> bool) -> bool {
    !cfg!(target_os = "macos") && !set(ADOPTED_ENV)
}

/// The directories only `current` names, in its order, then `login`'s, each
/// once. Relative and empty entries, which name a different directory wherever
/// a program runs, are dropped from both.
fn merged(current: Option<&OsStr>, login: &OsStr) -> Option<OsString> {
    let login: Vec<_> = std::env::split_paths(login).collect();
    let in_login: HashSet<_> = login.iter().cloned().collect();
    let mut seen = HashSet::new();
    let directories = current
        .into_iter()
        .flat_map(std::env::split_paths)
        .filter(|directory| !in_login.contains(directory))
        .chain(login)
        .filter(|directory| directory.is_absolute() && seen.insert(directory.clone()));
    std::env::join_paths(directories).ok()
}

/// The `PATH` `shell` sets up as an interactive login, as sshd starts it,
/// or `None` if it does not say within `timeout`. Shells that refuse a
/// combined login and command invocation, such as tcsh, are asked
/// interactively instead, within the same budget.
///
/// This runs before the daemon has threads, so it waits on the shell's output
/// directly rather than through a runtime or a reader thread.
fn read(shell: &Path, home: &Path, timeout: Duration) -> Option<OsString> {
    let deadline = Instant::now() + timeout;
    ["-il", "-i"]
        .into_iter()
        .find_map(|mode| read_in_mode(shell, mode, home, deadline))
}

fn read_in_mode(shell: &Path, mode: &str, home: &Path, deadline: Instant) -> Option<OsString> {
    if Instant::now() >= deadline {
        return None;
    }
    let mut command = Command::new(shell);
    command
        .args([mode, "-c", SHELL_PATH_COMMAND])
        .env("HOME", home)
        .current_dir(home);
    for name in SCRUBBED_ENVIRONMENT
        .iter()
        .chain(SHELL_SCRUBBED_ENVIRONMENT)
        .chain(LOGIN_SESSION_ENVIRONMENT)
    {
        command.env_remove(name);
    }
    for (name, _) in std::env::vars_os() {
        if name.to_str().is_some_and(crate::agent::is_session_variable) {
            command.env_remove(name);
        }
    }
    // A startup file may leave something running outside the group, beyond
    // the reach of the kill below. It must hold none of the update guards,
    // fences and leases a launcher hands the daemon.
    crate::install_guard::sanitize_child_environment(&mut command);
    // Its own session, and so its own process group for the kill below, with
    // no controlling terminal: a daemon started from a terminal still has
    // one, and a startup file reading it would stop the shell until the
    // deadline.
    // SAFETY: the hook performs only the close_range and setsid syscalls
    // before exec.
    unsafe {
        command.pre_exec(|| {
            crate::daemon::seal_inherited_descriptors()?;
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = command.spawn().ok()?;
    let mut stdout = child.stdout.take()?;
    let path = read_value(&mut stdout, deadline);
    drop(stdout);
    // The shell and anything a startup file left running in its group go,
    // answered or not.
    // SAFETY: `killpg` only sends a signal. The group is the one this spawn
    // created, and its leader is not reaped until the wait below, so its id
    // cannot belong to anyone else yet.
    unsafe {
        libc::killpg(child.id() as libc::pid_t, libc::SIGKILL);
    }
    let _ = child.wait();
    path
}

/// Reads until the value arrives: a job a startup file left running may hold
/// the pipe open long after it.
fn read_value(stdout: &mut std::process::ChildStdout, deadline: Instant) -> Option<OsString> {
    let mut output = Vec::new();
    let mut chunk = [0; 4096];
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let mut poll = libc::pollfd {
            fd: stdout.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let milliseconds = remaining.as_millis().clamp(1, i32::MAX as u128) as i32;
        // SAFETY: `poll` reads one valid pollfd for the descriptor `stdout` owns.
        match unsafe { libc::poll(&mut poll, 1, milliseconds) } {
            0 => return None,
            ready if ready < 0 => {
                if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return None;
            }
            _ => {}
        }
        match stdout.read(&mut chunk) {
            Ok(0) | Err(_) => return None,
            Ok(read) => output.extend_from_slice(&chunk[..read]),
        }
        if let Some(path) = path_from_output(&output) {
            return Some(path);
        }
        if output.len() > MAX_SHELL_OUTPUT || Instant::now() >= deadline {
            return None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_login_order_wins_behind_what_only_the_launcher_chose() {
        // A desktop session's `/usr/bin` does not shadow a version manager's
        // shims the login puts ahead of it, while a test's own directory,
        // which the login lacks, stays first.
        assert_eq!(
            merged(
                Some(OsStr::new("/fixture/bin:/usr/bin:relative:/usr/local/bin")),
                OsStr::new("/home/a/.local/share/mise/shims:/usr/local/bin:/usr/bin:/home/a/.local/bin")
            ),
            Some(
                "/fixture/bin:/home/a/.local/share/mise/shims:/usr/local/bin:/usr/bin:/home/a/.local/bin"
                    .into()
            )
        );
        assert_eq!(
            merged(None, OsStr::new("/opt/x/bin:relative:/opt/x/bin")),
            Some("/opt/x/bin".into())
        );
    }

    #[test]
    fn every_launcher_reads_the_login_shell_once() {
        let with = |names: &'static [&'static str]| wanted(move |name| names.contains(&name));
        // A window from a desktop launcher names no SSH session, and its
        // `PATH` can lack the login directories all the same.
        assert_eq!(with(&[]), !cfg!(target_os = "macos"));
        assert_eq!(with(&["SSH_CONNECTION"]), !cfg!(target_os = "macos"));
        assert!(!with(&[ADOPTED_ENV]));
        assert!(!with(&["SSH_CONNECTION", ADOPTED_ENV]));
    }

    #[test]
    fn a_login_shell_reads_its_login_startup() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join(".profile"),
            "echo welcome\nPATH=\"$HOME/from-profile:$PATH\"\n",
        )
        .unwrap();
        let path = read(Path::new("/bin/sh"), home.path(), Duration::from_secs(10)).unwrap();
        let first = std::env::split_paths(&path).next().unwrap();
        assert_eq!(first, home.path().join("from-profile"), "{path:?}");
    }

    #[test]
    fn a_shell_that_never_answers_is_abandoned_on_time() {
        let directory = tempfile::tempdir().unwrap();
        let shell = directory.path().join("shell");
        // Holds its output open in the background as a startup file's job
        // might, and never prints a value.
        std::fs::write(&shell, "#!/bin/sh\nsleep 30 &\nexec sleep 30\n").unwrap();
        std::fs::set_permissions(&shell, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        let started = Instant::now();
        assert_eq!(
            read(&shell, directory.path(), Duration::from_millis(300)),
            None
        );
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn a_shell_that_refuses_a_login_command_is_asked_interactively() {
        let directory = tempfile::tempdir().unwrap();
        let shell = directory.path().join("shell");
        // Like tcsh, which rejects `-il` with `-c`.
        std::fs::write(
            &shell,
            "#!/bin/sh\n[ \"$1\" = -il ] && exit 1\nprintf '__cagents_path__%d:%s\\0' 10 /opt/i/bin\n",
        )
        .unwrap();
        std::fs::set_permissions(&shell, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        assert_eq!(
            read(&shell, directory.path(), Duration::from_secs(10)),
            Some("/opt/i/bin".into())
        );
    }

    #[test]
    fn a_value_arrives_before_a_background_job_lets_go() {
        let directory = tempfile::tempdir().unwrap();
        let shell = directory.path().join("shell");
        // Ignores its arguments, prints a value as the probe command would,
        // and leaves a job holding the pipe.
        std::fs::write(
            &shell,
            "#!/bin/sh\nsleep 30 &\nprintf 'noise__cagents_path__%d:%s\\0' 13 /opt/demo/bin\nexec sleep 30\n",
        )
        .unwrap();
        std::fs::set_permissions(&shell, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        let started = Instant::now();
        assert_eq!(
            read(&shell, directory.path(), Duration::from_secs(10)),
            Some("/opt/demo/bin".into())
        );
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
