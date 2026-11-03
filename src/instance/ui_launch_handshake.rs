use std::{ffi::OsStr, process::Command};

pub const UI_LAUNCH_HANDSHAKE_ENV: &str = "CAGENTS_UI_LAUNCH_HANDSHAKE";
pub const UI_LAUNCH_UPDATE_MODE_ENV: &str = "CAGENTS_UI_LAUNCH_UPDATE_MODE";
const UI_LAUNCH_UPDATE_MODE: &str = "update-retry-v1";

/// Keep the final ready/error handshake readable by older UIs. Only newer UIs
/// consume the separate update capability; an older target simply ignores it.
pub(crate) fn configure_ui_launch_handshake(command: &mut Command, update: bool) {
    command.env(UI_LAUNCH_HANDSHAKE_ENV, "1");
    if update {
        command.env(UI_LAUNCH_UPDATE_MODE_ENV, UI_LAUNCH_UPDATE_MODE);
    } else {
        command.env_remove(UI_LAUNCH_UPDATE_MODE_ENV);
    }
}

pub(super) fn ui_launch_handshake_mode(
    handshake: Option<&OsStr>,
    update_mode: Option<&OsStr>,
) -> Option<bool> {
    match handshake {
        Some(value) if value == "1" => Some(update_mode == Some(OsStr::new(UI_LAUNCH_UPDATE_MODE))),
        // Accept earlier update coordinators without making this the
        // value new coordinators send to potentially older target UIs.
        Some(value) if value == UI_LAUNCH_UPDATE_MODE => Some(true),
        _ => None,
    }
}

pub(super) fn current_ui_launch_handshake_mode() -> Option<bool> {
    ui_launch_handshake_mode(
        std::env::var_os(UI_LAUNCH_HANDSHAKE_ENV).as_deref(),
        std::env::var_os(UI_LAUNCH_UPDATE_MODE_ENV).as_deref(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_and_both_update_coordinator_versions_remain_distinct() {
        assert_eq!(
            ui_launch_handshake_mode(Some("1".as_ref()), None),
            Some(false)
        );
        assert_eq!(
            ui_launch_handshake_mode(Some("1".as_ref()), Some(UI_LAUNCH_UPDATE_MODE.as_ref())),
            Some(true)
        );
        assert_eq!(
            ui_launch_handshake_mode(Some(UI_LAUNCH_UPDATE_MODE.as_ref()), None),
            Some(true)
        );
    }

    #[test]
    fn an_update_capability_alone_does_not_request_a_handshake() {
        for handshake in [None, Some("".as_ref()), Some("2".as_ref())] {
            assert_eq!(
                ui_launch_handshake_mode(handshake, Some(UI_LAUNCH_UPDATE_MODE.as_ref())),
                None
            );
        }
    }

    #[test]
    fn only_the_exact_update_mode_promotes_a_legacy_handshake() {
        for mode in [
            None,
            Some("".as_ref()),
            Some("1".as_ref()),
            Some("update".as_ref()),
        ] {
            assert_eq!(
                ui_launch_handshake_mode(Some("1".as_ref()), mode),
                Some(false)
            );
        }
    }

    #[test]
    fn configuring_an_ordinary_launch_removes_a_previously_set_update_mode() {
        let mut command = Command::new("/bin/sh");
        configure_ui_launch_handshake(&mut command, true);
        configure_ui_launch_handshake(&mut command, false);
        let mode = command
            .get_envs()
            .find(|(name, _)| *name == UI_LAUNCH_UPDATE_MODE_ENV)
            .unwrap();
        assert_eq!(mode.1, None);
        assert_eq!(
            command
                .get_envs()
                .find(|(name, _)| *name == UI_LAUNCH_HANDSHAKE_ENV)
                .unwrap()
                .1,
            Some(OsStr::new("1"))
        );
    }

    #[test]
    fn configuring_update_preserves_arguments_directory_and_unrelated_context() {
        let mut command = Command::new("/bin/sh");
        command
            .arg("unchanged-argument")
            .current_dir("/")
            .env("FIXTURE_CONTEXT", "retained");
        configure_ui_launch_handshake(&mut command, true);
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            [OsStr::new("unchanged-argument")]
        );
        assert_eq!(command.get_current_dir(), Some(std::path::Path::new("/")));
        assert_eq!(
            command
                .get_envs()
                .find(|(name, _)| *name == "FIXTURE_CONTEXT")
                .unwrap()
                .1,
            Some(OsStr::new("retained"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn update_launch_is_accepted_by_an_older_literal_one_only_ui() {
        let mut command = Command::new("/bin/sh");
        command.args([
            "-c",
            "test \"$CAGENTS_UI_LAUNCH_HANDSHAKE\" = 1 || exit 97; printf 'ready\\n'",
            "legacy-ui-fixture",
        ]);
        configure_ui_launch_handshake(&mut command, true);
        let output = command.output().unwrap();
        assert!(output.status.success(), "legacy handshake was refused");
        assert_eq!(output.stdout, b"ready\n");
        assert!(output.stderr.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn ordinary_child_cannot_inherit_the_update_opt_in() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "test \"$CAGENTS_UI_LAUNCH_HANDSHAKE\" = 1 || exit 97; test \"${CAGENTS_UI_LAUNCH_UPDATE_MODE+x}\" != x || exit 98; printf 'ready\\n'", "ordinary-ui-fixture"]);
        command.env(UI_LAUNCH_UPDATE_MODE_ENV, UI_LAUNCH_UPDATE_MODE);
        configure_ui_launch_handshake(&mut command, false);
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "ordinary launch retained update mode"
        );
        assert_eq!(output.stdout, b"ready\n");
        assert!(output.stderr.is_empty());
    }
}
