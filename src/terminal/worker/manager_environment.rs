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
use std::{process::Command, process::Stdio};

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
    match current() {
        Some(environment) => apply(command, &not_the_launchs(environment, launch)),
        None => tracing::debug!(
            "the user manager's environment is unavailable; the terminal keeps the daemon's"
        ),
    }
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
fn current() -> Option<Vec<String>> {
    let output = Command::new("busctl")
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
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|output| output.status.success())?;
    parse(&output.stdout)
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
