//! The `PATH` a daemon adopts from its user's login shell.
//!
//! Whatever starts the daemon hands it its own `PATH`, and on Linux that is
//! often not the one a terminal sees. sshd runs a command, such as a device
//! window's `cagents daemon bridge`, without the login startup files an
//! interactive SSH session reads. A window started by a desktop launcher, or
//! by a session whose login shell is not POSIX, such as fish, carries what the
//! display manager set, which commonly lacks `~/.local/bin` and the other
//! directories installers add in login files. The daemon reads the login
//! shell once for its inherited environment, including for terminals. Native
//! provider discovery also reads interactive startup and can be refreshed
//! while the daemon is running.

use std::{
    collections::HashSet,
    ffi::{OsStr, OsString},
    io::Read,
    os::{fd::AsRawFd, unix::process::CommandExt},
    path::{Path, PathBuf},
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
/// complete, such as a test's private daemon, sets it to skip startup adoption.
pub const ADOPTED_ENV: &str = "CAGENTS_LOGIN_PATH_ADOPTED";

#[cfg(target_os = "linux")]
static DAEMON_LAUNCHER_PATH: std::sync::OnceLock<Option<OsString>> = std::sync::OnceLock::new();

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
    let inherited = std::env::var_os("PATH");
    #[cfg(target_os = "linux")]
    DAEMON_LAUNCHER_PATH.get_or_init(|| inherited.clone());
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
    if let Some(path) = merged(inherited.as_deref(), &login) {
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
    merge_paths(
        current
            .into_iter()
            .flat_map(std::env::split_paths)
            .filter(|directory| directory.is_absolute()),
        std::env::split_paths(login).filter(|directory| directory.is_absolute()),
    )
}

/// A recorded selection keeps its order and gains any missing absolute
/// shell directories. An ambient launch follows the freshly discovered
/// shell order without giving relative entries higher precedence.
#[cfg(all(feature = "host", target_os = "linux"))]
pub fn merge_provider_path(
    recorded: Option<&str>,
    launcher: Option<&str>,
    shell: &OsStr,
) -> Option<String> {
    let shell: Vec<_> = std::env::split_paths(shell)
        .filter(|directory| directory.is_absolute() && directory.to_str().is_some())
        .collect();
    let directories = if let Some(recorded) = recorded.filter(|&path| Some(path) != launcher) {
        std::env::split_paths(recorded).chain(shell).collect()
    } else {
        restore_relative_entries(launcher, shell)
    };
    let mut seen = HashSet::new();
    let directories = directories
        .into_iter()
        .filter(|directory| seen.insert(directory.clone()));
    let path = std::env::join_paths(directories).ok()?.into_string().ok()?;
    (!path.is_empty()).then_some(path)
}

/// Keep each relative entry behind the surviving directories that originally
/// took precedence over it. A shell can reorder those directories, so use
/// the furthest position reached in its refreshed order.
#[cfg(all(feature = "host", target_os = "linux"))]
fn restore_relative_entries(launcher: Option<&str>, shell: Vec<PathBuf>) -> Vec<PathBuf> {
    let Some(launcher) = launcher else {
        return shell;
    };
    if !std::env::split_paths(launcher).any(|path| path.is_relative()) {
        return shell;
    }
    let mut after = std::collections::HashMap::new();
    for (index, path) in shell.iter().enumerate() {
        after.entry(path.as_path()).or_insert(index + 1);
    }
    let mut relative = vec![Vec::new(); shell.len() + 1];
    let mut position = 0;
    for path in std::env::split_paths(launcher) {
        if path.is_relative() {
            relative[position].push(path);
        } else if let Some(&next) = after.get(path.as_path()) {
            position = position.max(next);
        }
    }
    relative
        .into_iter()
        .zip(shell.into_iter().map(Some).chain([None]))
        .flat_map(|(relative, absolute)| relative.into_iter().chain(absolute))
        .collect()
}

fn merge_paths(
    current: impl Iterator<Item = PathBuf>,
    login: impl Iterator<Item = PathBuf>,
) -> Option<OsString> {
    let login: Vec<_> = login.collect();
    let in_login: HashSet<_> = login.iter().cloned().collect();
    let mut seen = HashSet::new();
    let directories = current
        .filter(|directory| !in_login.contains(directory))
        .chain(login)
        .filter(|directory| seen.insert(directory.clone()));
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
    let inherited = std::env::var_os("PATH");
    let config_home = std::env::var_os("XDG_CONFIG_HOME");
    ["-il", "-i"].into_iter().find_map(|mode| {
        read_in_mode(
            shell,
            mode,
            home,
            inherited.as_deref(),
            config_home.as_deref(),
            deadline,
        )
    })
}

/// Accounts and native starts begin with the original launcher PATH, so
/// removing a directory from shell startup also removes it from discovery.
#[cfg(target_os = "linux")]
pub(super) fn launcher_path() -> Option<OsString> {
    DAEMON_LAUNCHER_PATH
        .get()
        .cloned()
        .unwrap_or_else(|| std::env::var_os("PATH"))
}

#[cfg(target_os = "linux")]
pub(super) fn discovery_path(
    inherited: Option<&OsStr>,
    config_home: Option<&OsStr>,
) -> Option<OsString> {
    let shell = crate::model::login_shell().ok()?;
    let home = std::env::var_os("HOME")?;
    discover(
        Path::new(&shell),
        Path::new(&home),
        inherited,
        config_home,
        SHELL_TIMEOUT,
    )
}

#[cfg(target_os = "linux")]
fn discover(
    shell: &Path,
    home: &Path,
    inherited: Option<&OsStr>,
    config_home: Option<&OsStr>,
    timeout: Duration,
) -> Option<OsString> {
    // Start where a login leaves off, then let interactive startup make
    // its own changes. Each mode keeps its original time budget so a slow
    // but successful profile or interactive setup still supplies its PATH.
    let login = read_in_mode(
        shell,
        "-il",
        home,
        inherited,
        config_home,
        Instant::now() + timeout,
    )
    .and_then(|path| merged(inherited, &path));
    let interactive = read_in_mode(
        shell,
        "-i",
        home,
        login.as_deref().or(inherited),
        config_home,
        Instant::now() + timeout,
    );
    interactive.or(login)
}

fn read_in_mode(
    shell: &Path,
    mode: &str,
    home: &Path,
    inherited: Option<&OsStr>,
    config_home: Option<&OsStr>,
    deadline: Instant,
) -> Option<OsString> {
    if Instant::now() >= deadline {
        return None;
    }
    let mut command = Command::new(shell);
    command
        .args([mode, "-c", SHELL_PATH_COMMAND])
        .env("HOME", home)
        .current_dir(home);
    for (key, value) in [("PATH", inherited), ("XDG_CONFIG_HOME", config_home)] {
        match value {
            Some(value) => command.env(key, value),
            None => command.env_remove(key),
        };
    }
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
        if remaining.is_zero() {
            return None;
        }
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

    #[cfg(target_os = "linux")]
    #[test]
    fn discovery_layers_interactive_tools_over_current_login_startup() {
        let home = tempfile::tempdir().unwrap();
        let shell = home.path().join("shell");
        // Model bash with disjoint login and interactive files. The latter
        // needs the version-manager directory put on PATH by the former.
        std::fs::write(
            &shell,
            r#"#!/bin/sh
case "$1" in
    -il)
        [ ! -d "$HOME/.local/bin" ] || PATH="$HOME/.local/bin:$PATH"
        PATH="$HOME/login/bin:$PATH"
        ;;
    -i)
        case "$PATH" in *"$HOME/login/bin"*) ;; *) exit 2 ;; esac
        printf 'startup message\0'
        PATH="$HOME/interactive/bin:$PATH"
        [ -z "$XDG_CONFIG_HOME" ] || PATH="$XDG_CONFIG_HOME/tools:$PATH"
        ;;
    *) exit 3 ;;
esac
eval "$3"
"#,
        )
        .unwrap();
        std::fs::set_permissions(&shell, std::os::unix::fs::PermissionsExt::from_mode(0o700))
            .unwrap();
        let root = home.path();
        let launcher = OsStr::new("/usr/bin:/bin");
        let default_discovery =
            || discover(&shell, root, Some(launcher), None, Duration::from_secs(5)).unwrap();
        let before: Vec<_> = std::env::split_paths(&default_discovery()).collect();
        assert_eq!(
            before,
            [
                root.join("interactive/bin"),
                root.join("login/bin"),
                "/usr/bin".into(),
                "/bin".into(),
            ]
        );
        let installed = root.join(".local/bin");
        std::fs::create_dir_all(&installed).unwrap();
        let cli = installed.join("claude");
        std::fs::write(&cli, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&cli, std::os::unix::fs::PermissionsExt::from_mode(0o700))
            .unwrap();
        let after = default_discovery();
        assert_eq!(
            super::super::find_executable("claude", Some(&after)),
            Some(cli)
        );
        assert_eq!(
            std::env::split_paths(&after).collect::<Vec<_>>(),
            [
                root.join("interactive/bin"),
                root.join("login/bin"),
                installed,
                "/usr/bin".into(),
                "/bin".into(),
            ]
        );
        let profile = root.join("window-config");
        let configured = discover(
            &shell,
            root,
            Some(launcher),
            Some(profile.as_os_str()),
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(
            std::env::split_paths(&configured).next(),
            Some(profile.join("tools")),
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
