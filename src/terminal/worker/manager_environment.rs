//! The graphical session's variables for a daemon started at boot.
//!
//! A daemon the boot unit started holds the systemd user manager's
//! environment from before anyone logged in. A graphical login then imports
//! its display, desktop and agent variables into the user manager
//! (`dbus-update-activation-environment --systemd`, `systemctl --user
//! import-environment`). So each terminal such a daemon launches takes those
//! variables from the user manager as it is now, rather than from the
//! daemon's own environment at boot.
//!
//! Only these variables: the rest of the user manager's environment is not
//! the session's, and the daemon's own `PATH` and the like stay as they are.
use portable_pty::CommandBuilder;
use std::{
    io::Read,
    process::{Command, Stdio},
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

/// What a graphical login imports into the user manager for its programs to
/// reach the display, the desktop and the login's agents.
pub(super) const SESSION_VARIABLES: &[&str] = &[
    "DISPLAY",
    "WAYLAND_DISPLAY",
    "XAUTHORITY",
    "XDG_CURRENT_DESKTOP",
    "XDG_SESSION_DESKTOP",
    "XDG_SESSION_TYPE",
    "DESKTOP_SESSION",
    "SSH_AUTH_SOCK",
];

/// Sets [`SESSION_VARIABLES`] on `command` from the user manager, when this
/// daemon was started at boot. Unchanged when the user manager cannot say;
/// a variable the launch sets or unsets itself is the launch's.
pub(super) fn refresh(command: &mut CommandBuilder, launch: &crate::model::LaunchRecipe) {
    if !crate::instance::daemon_started_at_boot() {
        return;
    }
    match cached(Instant::now(), current) {
        Some(environment) => apply(command, &not_the_launchs(environment, launch)),
        None => {
            if !UNAVAILABLE_REPORTED.swap(true, Ordering::Relaxed) {
                tracing::warn!(
                    "the user manager's environment is unavailable; terminals keep the daemon's display and agent variables until it answers"
                );
            }
        }
    }
}

static UNAVAILABLE_REPORTED: AtomicBool = AtomicBool::new(false);

/// How long one reading serves the terminals that start after it, such as a
/// window's restored panes, which would otherwise each ask in turn.
const FRESH: Duration = Duration::from_secs(1);
/// How long a failed reading stands, so a wedged bus costs one deadline
/// rather than one per terminal.
const FAILED_FRESH: Duration = Duration::from_secs(10);
/// The whole reading's limit. busctl's own timeout bounds each of its calls,
/// not the process.
const DEADLINE: Duration = Duration::from_secs(2);

/// The last reading, or a new one from `read` once it is stale. Concurrent
/// launches wait for the one reading in progress rather than start their own.
fn cached(now: Instant, read: impl FnOnce() -> Option<Vec<String>>) -> Option<Vec<String>> {
    static LAST: Mutex<Option<(Instant, Option<Vec<String>>)>> = Mutex::new(None);
    let mut last = LAST.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    cached_in(&mut last, now, read)
}

fn cached_in(
    last: &mut Option<(Instant, Option<Vec<String>>)>,
    now: Instant,
    read: impl FnOnce() -> Option<Vec<String>>,
) -> Option<Vec<String>> {
    if let Some((at, reading)) = last.as_ref() {
        let fresh = if reading.is_some() {
            FRESH
        } else {
            FAILED_FRESH
        };
        if now.saturating_duration_since(*at) < fresh {
            return reading.clone();
        }
    }
    let reading = read();
    if reading.is_some() {
        UNAVAILABLE_REPORTED.store(false, Ordering::Relaxed);
    }
    *last = Some((now, reading.clone()));
    reading
}

/// `environment` without the variables `launch` sets or unsets itself.
pub(super) fn not_the_launchs(
    environment: Vec<String>,
    launch: &crate::model::LaunchRecipe,
) -> Vec<String> {
    environment
        .into_iter()
        .filter(|entry| {
            entry.split_once('=').is_none_or(|(name, _)| {
                !launch.environment.contains_key(name) && !launch.unset_environment.contains(name)
            })
        })
        .collect()
}

/// Each session variable the user manager has, in place of the daemon's.
///
/// One it lacks keeps the daemon's value: a daemon started at boot has none,
/// so a terminal after a logout gets none either, while a replacement an
/// update started from a login keeps what that login gave it.
pub(super) fn apply(command: &mut CommandBuilder, environment: &[String]) {
    for name in SESSION_VARIABLES {
        let value = environment.iter().find_map(|entry| {
            entry
                .strip_prefix(name)
                .and_then(|rest| rest.strip_prefix('='))
        });
        if let Some(value) = value {
            command.env(name, value);
        }
    }
}

/// The user manager's `Environment` property, `NAME=value` entries.
///
/// Read through `busctl`, as process scopes are, from the user's own manager
/// rather than whatever bus the daemon's environment names. The property is
/// a string array, so values need no unquoting, unlike `systemctl
/// show-environment`'s output.
///
/// The user manager's runtime directory is the user's own, not the
/// isolated one a development or test daemon may have in `XDG_RUNTIME_DIR`.
fn current() -> Option<Vec<String>> {
    let mut child = Command::new("busctl")
        .env(
            "XDG_RUNTIME_DIR",
            format!("/run/user/{}", crate::daemon::current_uid()),
        )
        .env_remove("DBUS_SESSION_BUS_ADDRESS")
        .args([
            "--user",
            "--timeout=2s",
            "--json=short",
            "get-property",
            "org.freedesktop.systemd1",
            "/org/freedesktop/systemd1",
            "org.freedesktop.systemd1.Manager",
            "Environment",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    // Read beside the wait, so a large environment cannot fill the pipe and
    // stall busctl before it exits.
    let reader = std::thread::Builder::new()
        .name("cagents-manager-env".into())
        .spawn(move || {
            let mut output = Vec::new();
            stdout.read_to_end(&mut output).map(|_| output)
        })
        .ok();
    let deadline = Instant::now() + DEADLINE;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            _ => {
                // Not yet reaped, so the PID is still this child's.
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let output = reader?.join().ok()?.ok()?;
    status.filter(|status| status.success())?;
    parse(&output)
}

/// `busctl --json` prints a property as `{"type":"as","data":[...]}`.
pub(super) fn parse(output: &[u8]) -> Option<Vec<String>> {
    #[derive(serde::Deserialize)]
    struct Property {
        r#type: String,
        data: Vec<String>,
    }
    serde_json::from_slice::<Property>(output)
        .ok()
        .filter(|property| property.r#type == "as")
        .map(|property| property.data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_reading_serves_launches_close_behind_it() {
        let start = Instant::now();
        let mut last = None;
        let reading = || Some(vec!["DISPLAY=:0".to_owned()]);
        assert_eq!(cached_in(&mut last, start, reading), reading());
        // Within a second the reading stands; `read` is not called.
        let unread = || -> Option<Vec<String>> { panic!("read again") };
        assert_eq!(
            cached_in(&mut last, start + Duration::from_millis(500), unread),
            reading()
        );
        assert_eq!(
            cached_in(&mut last, start + Duration::from_secs(2), || None),
            None
        );
        // A failure stands longer, so a wedged bus is asked once.
        assert_eq!(
            cached_in(&mut last, start + Duration::from_secs(8), unread),
            None
        );
        assert_eq!(
            cached_in(&mut last, start + Duration::from_secs(13), reading),
            reading()
        );
    }

    #[test]
    fn busctl_output_is_read_as_entries() {
        let output = br#"{"type":"as","data":["HOME=/home/u","DISPLAY=:0","ODD=a b=\"c\""]}"#;
        assert_eq!(
            parse(output),
            Some(vec![
                "HOME=/home/u".to_owned(),
                "DISPLAY=:0".to_owned(),
                "ODD=a b=\"c\"".to_owned(),
            ])
        );
        assert_eq!(parse(br#"{"type":"s","data":"x"}"#), None);
        assert_eq!(parse(b"Failed to connect"), None);
    }

    #[test]
    fn a_launch_keeps_the_variables_it_sets_or_unsets() {
        let launch = crate::model::LaunchRecipe {
            program: "/bin/sh".into(),
            arguments: Vec::new(),
            working_directory: "/".into(),
            environment: [("DISPLAY".to_owned(), ":5".to_owned())].into(),
            unset_environment: ["SSH_AUTH_SOCK".to_owned()].into(),
        };
        assert_eq!(
            not_the_launchs(
                vec![
                    "DISPLAY=:0".into(),
                    "SSH_AUTH_SOCK=/run/agent".into(),
                    "WAYLAND_DISPLAY=wayland-1".into(),
                ],
                &launch
            ),
            vec!["WAYLAND_DISPLAY=wayland-1".to_owned()]
        );
    }

    #[test]
    fn the_session_variables_follow_the_user_manager() {
        let mut command = CommandBuilder::new("/bin/sh");
        command.env("DISPLAY", ":9");
        command.env("SSH_AUTH_SOCK", "/run/old-agent");
        command.env("PATH", "/usr/bin");
        command.env("WAYLAND_DISPLAYX", "kept");
        apply(
            &mut command,
            &[
                "WAYLAND_DISPLAY=wayland-1".to_owned(),
                "DISPLAY=:0".to_owned(),
                "XDG_CURRENT_DESKTOP=niri".to_owned(),
                "PATH=/manager/bin".to_owned(),
                "WAYLAND_DISPLAYX=manager".to_owned(),
            ],
        );
        let get = |name: &str| command.get_env(name).and_then(|value| value.to_str());
        assert_eq!(get("DISPLAY"), Some(":0"));
        assert_eq!(get("WAYLAND_DISPLAY"), Some("wayland-1"));
        assert_eq!(get("XDG_CURRENT_DESKTOP"), Some("niri"));
        // The user manager has none: the daemon's own stays.
        assert_eq!(get("SSH_AUTH_SOCK"), Some("/run/old-agent"));
        // Not a session variable, nor one by prefix.
        assert_eq!(get("PATH"), Some("/usr/bin"));
        assert_eq!(get("WAYLAND_DISPLAYX"), Some("kept"));
    }
}
