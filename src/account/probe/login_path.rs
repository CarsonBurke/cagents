//! The `PATH` a daemon started by an SSH command adopts.
//!
//! sshd runs a command, such as a device window's `cagents daemon bridge`,
//! without the login startup that an interactive SSH session gets, so the
//! daemon it starts would inherit a `PATH` of system directories only.
//! Installers commonly add `~/.local/bin` and similar directories in login
//! files such as `~/.profile`, and terminals, provider CLIs and conversations
//! on the device would all miss them. Like the server VS Code starts over SSH,
//! the daemon reads its user's login shell once at startup instead.

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

/// What a login shell is not told, so that startup files guarded on them, such
/// as an automatic `tmux attach` for SSH logins, stay out of the way.
const SSH_SESSION_ENVIRONMENT: &[&str] = &["SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY"];

/// Set once a daemon has read the login shell, so that what it starts, such as
/// the daemon an update hands over to or one started in its terminals, does
/// not read it again. The `PATH` they inherit already holds the answer.
const ADOPTED_ENV: &str = "CAGENTS_LOGIN_PATH_ADOPTED";

/// Puts the login shell's `PATH` ahead of this process's own when an SSH
/// command started it. Elsewhere, and when the shell does not say, the `PATH`
/// is left as it is: a desktop session already ran the login startup, and
/// macOS reads the shell for each launch instead.
///
/// # Safety
///
/// Must be called before the process starts a second thread: it changes the
/// process environment, which another thread may be reading.
pub unsafe fn adopt() {
    if cfg!(target_os = "macos")
        || std::env::var_os("SSH_CONNECTION").is_none()
        || std::env::var_os(ADOPTED_ENV).is_some()
    {
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
    if let Some(path) = merged(&login, std::env::var_os("PATH").as_deref()) {
        // SAFETY: the caller guarantees the process is still single-threaded.
        unsafe { std::env::set_var("PATH", path) };
    }
}

/// `login`'s directories, then those of `current` it lacks, each once. As in
/// an interactive login, the login's order wins, and relative or empty
/// entries, which name a different directory wherever a program runs, are
/// dropped from both.
fn merged(login: &OsStr, current: Option<&OsStr>) -> Option<OsString> {
    let mut seen = HashSet::new();
    let directories = std::env::split_paths(login)
        .chain(current.into_iter().flat_map(std::env::split_paths))
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
        .chain(SSH_SESSION_ENVIRONMENT)
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
    // SAFETY: the hook performs only the close_range syscall before exec.
    unsafe {
        command.pre_exec(crate::daemon::seal_inherited_descriptors);
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0);
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
    fn the_login_path_leads_and_nothing_repeats() {
        assert_eq!(
            merged(
                OsStr::new("/home/a/.local/bin:/usr/bin:relative"),
                Some(OsStr::new("/usr/local/bin:/usr/bin:/home/a/.local/bin"))
            ),
            Some("/home/a/.local/bin:/usr/bin:/usr/local/bin".into())
        );
        assert_eq!(
            merged(OsStr::new("/opt/x/bin"), None),
            Some("/opt/x/bin".into())
        );
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
