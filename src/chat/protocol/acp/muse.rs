//! Muse's ACP server, `muse acp` (checked against Muse 1.4.23 and 1.4.24).
//!
//! A Muse conversation's launch is the terminal command it runs as, or
//! would resume with: `muse [OPTIONS]`, or `muse resume ID [OPTIONS]`. Native
//! chat runs the same installation as `muse acp`, with the options that
//! command takes, and applies the rest as session settings.
//!
//! - `acp` must come first: Muse refuses a root option before a subcommand.
//!   A `muse` that is a wrapper adding root options therefore cannot serve
//!   ACP, so the launcher it wraps, `muse-upstream`, is run when present.
//! - Approval is chosen per session, through the session's mode, so
//!   `--approval-mode` becomes the matching mode. The sandbox is fixed for
//!   the server's lifetime and keeps its own options.
//! - Contributor mode, which lets Meta use the conversation for product
//!   improvement, is always off.
use super::{Profile, Settings, Startup, auth_required};
use crate::chat::Launch;
use crate::model::LaunchRecipe;

pub static PROFILE: Profile = Profile {
    label: "Muse",
    launch,
    max_image_bytes: crate::chat::ACP_MAX_IMAGE_BYTES,
    // By what each does, in Muse's words: "Ask for approval when no
    // permission rule covers the action", "Ask for approval only when the
    // action requires it", "Refuse any action no permission rule covers" and
    // "Run every action without asking for approval". Offered in the order
    // of the Claude modes they roughly correspond to, rather than Muse's
    // own, which leads with running everything unasked.
    modes: &[
        (PROMPT_UNMATCHED, "manual"),
        (ON_REQUEST, "auto"),
        (DENY_UNMATCHED, "dontAsk"),
        (ALLOW_ALL, "bypassPermissions"),
    ],
    offers_model: crate::agent::muse_offers_model,
    sign_in_required: auth_required,
};

/// Muse's ACP mode identifiers.
const ALLOW_ALL: &str = "allowAll";
const PROMPT_UNMATCHED: &str = "promptUnmatched";
const ON_REQUEST: &str = "onRequest";
const DENY_UNMATCHED: &str = "denyUnmatched";
const MODES: [&str; 4] = [ALLOW_ALL, PROMPT_UNMATCHED, ON_REQUEST, DENY_UNMATCHED];

/// `muse acp` options that set up the server and mean the same there.
const SERVER_VALUES: [&str; 5] = [
    "--provider",
    "--preset",
    "--config",
    "-c",
    "--sandbox-network",
];
const SERVER_SWITCHES: [&str; 5] = [
    "--disable-sandbox",
    "--disable-write",
    "--disable-shell",
    "--trust-workspace",
    "--disable-request-user-input",
];

fn launch(launch: &Launch) -> Result<Startup, String> {
    let mut arguments: Vec<String> = ["acp", "--contributor-mode", "off"]
        .map(String::from)
        .into();
    let mut settings = Settings::default();
    // An explicit `--approval-mode` outranks a switch that implies one.
    let mut implied_mode = None;
    let mut given = launch.recipe.arguments.iter();
    let mut positional = 0usize;
    while let Some(argument) = given.next() {
        let (name, inline) = match argument.split_once('=') {
            Some((name, value)) if name.starts_with('-') => (name, Some(value)),
            _ => (argument.as_str(), None),
        };
        let mut value = || -> Result<String, String> {
            match inline {
                Some(value) => Ok(value.to_owned()),
                None => given
                    .next()
                    .cloned()
                    .ok_or_else(|| format!("Muse's {name} option is missing its value.")),
            }
        };
        match name {
            "--model" => settings.model = Some(value()?),
            "--reasoning-effort" | "--effort" => settings.reasoning = Some(value()?),
            "--approval-mode" => settings.mode = Some(mode(&value()?)?),
            "--disable-approval" => implied_mode = Some(ALLOW_ALL),
            "--yolo" => {
                implied_mode = Some(ALLOW_ALL);
                arguments.extend(["--disable-sandbox".into(), "--trust-workspace".into()]);
            }
            "--contributor-mode" => {
                if value()? != "off" {
                    return Err(
                        "A native Muse chat runs with contributor mode off, so Meta \
                         does not use the conversation. Run Muse in a terminal to turn \
                         it on."
                            .into(),
                    );
                }
            }
            // Capability is on by default; the flag is kept for compatibility.
            "--subagent-worktree-isolation" => {}
            "--no-session-log" => {
                return Err("Muse keeps no conversation with --no-session-log, so it \
                     cannot be reopened as a chat. Run it in a terminal instead."
                    .into());
            }
            _ if SERVER_VALUES.contains(&name) => {
                let value = value()?;
                arguments.extend([name.to_owned(), value]);
            }
            _ if SERVER_SWITCHES.contains(&name) => arguments.push(name.to_owned()),
            _ if name.starts_with('-') => {
                return Err(format!(
                    "A native Muse chat can't use the launch option {name}. Run Muse in a \
                     terminal to use it."
                ));
            }
            // `resume` and the session it names; the conversation's own
            // identifier says which session to load.
            "resume" if positional == 0 => {
                positional += 1;
                if launch.session_id.is_none() {
                    return Err("Muse needs the conversation's exact session ID to open \
                         it as a chat."
                        .into());
                }
                let mut rest = given.clone();
                if rest
                    .next()
                    .is_some_and(|next| !next.starts_with('-') || next == "--last")
                {
                    given.next();
                }
            }
            _ => {
                return Err("A native Muse chat can't start from a prompt on Muse's \
                     command line. Start the chat, then send the prompt."
                    .into());
            }
        }
    }
    if settings.mode.is_none() {
        settings.mode = implied_mode.map(str::to_owned);
    }
    Ok(Startup {
        program: program(&launch.recipe),
        arguments,
        settings,
    })
}

/// The session mode an approval mode means. Muse's terminal spells
/// approval `never`, `on-request` and `untrusted`; its sessions call the same
/// postures modes, and a launch cagents wrote may name one of those, or the
/// cagents mode one means, as `agent spawn` does for "Deny unmatched
/// actions", which the terminal cannot name.
fn mode(approval: &str) -> Result<String, String> {
    let mode = match approval {
        "never" => ALLOW_ALL,
        "on-request" => ON_REQUEST,
        "untrusted" => PROMPT_UNMATCHED,
        mode if MODES.contains(&mode) => mode,
        // What a mode means, as cagents names it.
        meaning if MODES.contains(&PROFILE.own_mode(meaning)) => PROFILE.own_mode(meaning),
        _ => {
            return Err(format!(
                "Muse has no approval mode {approval:?}; choose manual, auto, dontAsk or bypassPermissions, or the terminal's never, on-request or untrusted."
            ));
        }
    };
    Ok(mode.to_owned())
}

/// The executable to serve ACP with: `muse-upstream` where the launch's
/// `muse` has one beside it or its `PATH` has one, else the launch's own.
fn program(recipe: &LaunchRecipe) -> String {
    // The runtime's own default when the launch has no PATH.
    let path = recipe
        .environment
        .get("PATH")
        .map_or("/usr/bin:/bin", String::as_str);
    crate::agent::Provider::Muse.subcommand_program(
        &recipe.program,
        &recipe.working_directory,
        path,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    const UPSTREAM: &str = "muse-upstream";

    fn launch_of(program: &str, arguments: &[&str], session: Option<&str>, path: &str) -> Launch {
        Launch {
            provider: crate::agent::Provider::Muse,
            recipe: LaunchRecipe {
                program: program.into(),
                arguments: arguments
                    .iter()
                    .map(|argument| (*argument).into())
                    .collect(),
                working_directory: "/".into(),
                environment: [("PATH".to_owned(), path.to_owned())].into(),
                unset_environment: Default::default(),
            },
            session_id: session.map(str::to_owned),
        }
    }

    fn startup(arguments: &[&str]) -> Result<Startup, String> {
        launch(&launch_of("muse", arguments, None, "/nonexistent"))
    }

    fn install(directory: &Path, name: &str) {
        let path = directory.join(name);
        std::fs::write(&path, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn serves_acp_first_with_contributor_mode_off() {
        let startup = startup(&[]).unwrap();
        assert_eq!(startup.arguments, ["acp", "--contributor-mode", "off"]);
        assert_eq!(startup.settings, Settings::default());
    }

    #[test]
    fn settings_become_session_settings_and_server_options_follow_acp() {
        let startup = startup(&[
            "--model",
            "muse-spark-1.2",
            "--effort=low",
            "--approval-mode",
            "never",
            "--disable-sandbox",
            "--sandbox-network=enabled",
            "-c",
            "key=1",
            "--contributor-mode",
            "off",
        ])
        .unwrap();
        assert_eq!(
            startup.arguments,
            [
                "acp",
                "--contributor-mode",
                "off",
                "--disable-sandbox",
                "--sandbox-network",
                "enabled",
                "-c",
                "key=1"
            ]
        );
        assert_eq!(
            startup.settings,
            Settings {
                model: Some("muse-spark-1.2".into()),
                reasoning: Some("low".into()),
                mode: Some("allowAll".into()),
            }
        );
    }

    #[test]
    fn approval_modes_map_to_session_modes() {
        for (approval, expected) in [
            ("never", "allowAll"),
            ("on-request", "onRequest"),
            ("untrusted", "promptUnmatched"),
            ("denyUnmatched", "denyUnmatched"),
            ("dontAsk", "denyUnmatched"),
            ("manual", "promptUnmatched"),
        ] {
            let startup = startup(&["--approval-mode", approval]).unwrap();
            assert_eq!(
                startup.settings.mode.as_deref(),
                Some(expected),
                "{approval}"
            );
        }
        assert!(startup(&["--approval-mode", "sometimes"]).is_err());
    }

    #[test]
    fn switches_imply_a_mode_an_explicit_one_overrides() {
        let yolo = startup(&["--yolo"]).unwrap();
        assert_eq!(yolo.settings.mode.as_deref(), Some("allowAll"));
        assert_eq!(
            yolo.arguments[3..],
            ["--disable-sandbox", "--trust-workspace"]
        );
        let explicit = startup(&["--disable-approval", "--approval-mode", "untrusted"]).unwrap();
        assert_eq!(explicit.settings.mode.as_deref(), Some("promptUnmatched"));
    }

    #[test]
    fn resume_form_loads_by_session_id() {
        let id = "01a1f155-1f9e-7081-b35f-88ead0a9187d";
        let resumed = launch(&launch_of(
            "muse",
            &["resume", id, "--model", "muse-spark-1.2"],
            Some(id),
            "/nonexistent",
        ))
        .unwrap();
        assert_eq!(resumed.arguments, ["acp", "--contributor-mode", "off"]);
        assert_eq!(resumed.settings.model.as_deref(), Some("muse-spark-1.2"));
        // Without the exact session, `resume` would silently open a new one.
        assert!(startup(&["resume", id]).is_err());
        assert!(startup(&["resume", "--last"]).is_err());
    }

    #[test]
    fn refuses_what_a_native_chat_cannot_honour() {
        for arguments in [
            &["--contributor-mode", "on"][..],
            &["--no-session-log"],
            &["--worktree"],
            &["--base-url", "https://example.invalid"],
            &["fix the build"],
            &["--model"],
        ] {
            assert!(startup(arguments).is_err(), "{arguments:?}");
        }
        assert!(startup(&["--subagent-worktree-isolation"]).is_ok());
    }

    #[test]
    fn prefers_the_upstream_launcher_to_a_wrapper() {
        let root = tempfile::tempdir().unwrap();
        let (wrapped, other) = (root.path().join("bin"), root.path().join("other"));
        std::fs::create_dir_all(&wrapped).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        install(&wrapped, "muse");
        let path = format!("{}:{}", other.display(), wrapped.display());
        let program = |name: &str| launch(&launch_of(name, &[], None, &path)).unwrap().program;
        // Only a wrapper: run it.
        assert_eq!(program("muse"), "muse");
        install(&wrapped, UPSTREAM);
        // On PATH, by name, as the runtime resolves it.
        assert_eq!(program("muse"), UPSTREAM);
        // Beside an explicit path, that one.
        let explicit = wrapped.join("muse").display().to_string();
        assert_eq!(
            program(&explicit),
            wrapped.join(UPSTREAM).display().to_string()
        );
        // An explicit upstream, or another executable, is kept.
        assert_eq!(program(UPSTREAM), UPSTREAM);
        assert_eq!(program("/opt/fake-acp-agent"), "/opt/fake-acp-agent");
    }
}
