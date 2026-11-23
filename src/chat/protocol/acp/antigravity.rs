//! Antigravity's ACP server, `agy acp`.
//!
//! A conversation's launch is the terminal command that runs it, `agy` with
//! its own options; this turns it into `agy acp` with the options that
//! command also takes, and the session settings `agy` would have started in.
//! Telemetry is always off, and cagents never names itself as anything else
//! to Antigravity or its service.
use super::{Profile, Settings, Startup};
use crate::chat::{self, Launch};

pub static PROFILE: Profile = Profile {
    label: "Antigravity",
    launch,
    max_image_bytes: chat::ACP_MAX_IMAGE_BYTES,
    modes: crate::agent::ANTIGRAVITY_MODES,
    offers_model: |_| true,
    sign_in_required: super::auth_required,
};

/// What `agy acp` takes as `agy` does, with a value.
const CARRIED_VALUES: &[&str] = &["--add-dir", "--agent", "--log-file"];
/// What `agy acp` takes as `agy` does, alone.
const CARRIED_SWITCHES: &[&str] = &["--dangerously-skip-permissions", "--sandbox"];

fn launch(launch: &Launch) -> Result<Startup, String> {
    let mut settings = Settings::default();
    let mut arguments = vec!["acp".to_owned(), "--disable-telemetry".to_owned()];
    let mut given = launch.recipe.arguments.iter();
    while let Some(argument) = given.next() {
        let (name, inline) = argument
            .split_once('=')
            .map_or((argument.as_str(), None), |(name, value)| {
                (name, Some(value))
            });
        let mut value = || -> Result<String, String> {
            match inline {
                Some(value) => Ok(value.to_owned()),
                None => given
                    .next()
                    .cloned()
                    .ok_or_else(|| format!("Missing value for {name}")),
            }
        };
        match name {
            "--model" => settings.model = Some(value()?),
            "--effort" => settings.reasoning = Some(value()?),
            "--mode" => settings.mode = Some(value()?),
            // The conversation is loaded by the launch's session ID, which
            // the command must name, so neither can open a different one.
            "--conversation" => {
                let named = value()?;
                if launch.session_id.as_deref() != Some(named.as_str()) {
                    return Err(format!(
                        "Antigravity's native chat cannot resume conversation {named}: it is not the conversation this chat holds."
                    ));
                }
            }
            "--continue" | "-c" => {
                return Err(
                    "Antigravity's native chat resumes a conversation by its ID, not as the most recent one."
                        .into(),
                );
            }
            _ if CARRIED_VALUES.contains(&name) => {
                let value = value()?;
                arguments.extend([name.to_owned(), value]);
            }
            _ if CARRIED_SWITCHES.contains(&name) && inline.is_none() => {
                arguments.push(name.to_owned());
            }
            _ => {
                return Err(format!(
                    "Antigravity's native chat cannot carry the option {name:?} from `agy`; run it in a terminal instead."
                ));
            }
        }
    }
    Ok(Startup {
        program: launch.recipe.program.clone(),
        arguments,
        settings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn launch_of(arguments: &[&str], session: Option<&str>) -> Launch {
        Launch {
            provider: crate::agent::Provider::Antigravity,
            recipe: crate::model::LaunchRecipe {
                program: "/usr/bin/agy".into(),
                arguments: arguments
                    .iter()
                    .map(|argument| (*argument).into())
                    .collect(),
                working_directory: "/work".into(),
                environment: Default::default(),
                unset_environment: Default::default(),
            },
            session_id: session.map(str::to_owned),
            draft_model: None,
        }
    }

    #[test]
    fn a_terminal_command_becomes_the_acp_server_with_telemetry_off() {
        let startup = launch(&launch_of(
            &[
                "--model",
                "gemini-3.8-flash",
                "--effort=low",
                "--mode",
                "accept-edits",
                "--add-dir",
                "/more",
                "--sandbox",
                "--conversation",
                "98608bab-5eb6-4cf5-8f9b-46019bf29df7",
            ],
            Some("98608bab-5eb6-4cf5-8f9b-46019bf29df7"),
        ))
        .unwrap();
        assert_eq!(startup.program, "/usr/bin/agy");
        assert_eq!(
            startup.arguments,
            [
                "acp",
                "--disable-telemetry",
                "--add-dir",
                "/more",
                "--sandbox"
            ]
        );
        assert_eq!(
            startup.settings,
            Settings {
                model: Some("gemini-3.8-flash".into()),
                reasoning: Some("low".into()),
                mode: Some("accept-edits".into()),
            }
        );
        assert!(
            !startup
                .arguments
                .iter()
                .any(|argument| argument.contains("user_agent")),
            "cagents never disguises itself"
        );
    }

    #[test]
    fn what_a_native_conversation_cannot_honour_is_refused() {
        for arguments in [
            &["--continue"][..],
            &["-c"],
            &["--print", "hi"],
            &["--settings", "{}"],
            &["--override_user_agent_name", "other"],
            &["--model"],
            &["a prompt"],
        ] {
            assert!(
                launch(&launch_of(arguments, None)).is_err(),
                "{arguments:?}"
            );
        }
    }

    #[test]
    fn a_conversation_the_chat_does_not_hold_is_never_opened_in_its_place() {
        let named = ["--conversation", "98608bab-5eb6-4cf5-8f9b-46019bf29df7"];
        for session in [None, Some("4b1f2c56-0d1e-4f4c-9a55-0f1e2d3c4b5a")] {
            assert!(launch(&launch_of(&named, session)).is_err(), "{session:?}");
        }
        assert!(launch(&launch_of(&named, Some(named[1]))).is_ok());
    }
}
