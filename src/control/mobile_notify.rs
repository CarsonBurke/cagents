use std::{
    io::{self, IsTerminal},
    path::Path,
};

use clap::{Subcommand, ValueEnum};
use serde_json::json;

use super::{AppError, CommandOutput, control_daemon_client};
use crate::{
    mobile::{
        notify::{self, Detail, Settings},
        pairing::AuthorizedKeys,
    },
    notifications::TestOutcome,
};

#[derive(Debug, Subcommand)]
pub enum MobileCommand {
    /// Manage optional phone alerts through an ntfy relay.
    Notify {
        #[command(subcommand)]
        command: NotifyCommand,
    },
}

impl MobileCommand {
    pub(super) fn uses_daemon(&self) -> bool {
        matches!(
            self,
            Self::Notify {
                command: NotifyCommand::Test
            }
        )
    }
}

#[derive(Debug, Subcommand)]
pub enum NotifyCommand {
    /// Show whether the relay is enabled and how much detail it shares.
    Status,
    /// Change the relay settings for this computer.
    #[command(arg_required_else_help = true)]
    Set {
        /// Enable or disable delivery.
        #[arg(long, action = clap::ArgAction::Set)]
        enabled: Option<bool>,
        /// The public HTTPS origin of the ntfy server.
        #[arg(long)]
        server: Option<String>,
        /// Show a generic alert or share the available conversation title.
        #[arg(long, value_enum)]
        detail: Option<DetailArg>,
        /// Also send alerts when a run finishes successfully.
        #[arg(long, action = clap::ArgAction::Set)]
        finished: Option<bool>,
        /// The name or SSH fingerprint of a paired phone.
        #[arg(long)]
        phone: Option<String>,
    },
    /// Show the selected phone's private topic in your terminal for ntfy setup.
    Subscribe,
    /// Ask the daemon to send one generic test notification.
    Test,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum DetailArg {
    Generic,
    Titles,
}

pub(super) fn execute(command: MobileCommand, json: bool) -> Result<CommandOutput, AppError> {
    let path =
        notify::settings_path().ok_or_else(|| error("Cannot find mobile notification settings"))?;
    let keys =
        || AuthorizedKeys::of_user().map_err(|_| error("Could not find your phone pairings"));
    execute_on(command, &path, keys, json)
}

fn execute_on(
    MobileCommand::Notify { command }: MobileCommand,
    path: &Path,
    keys: impl Fn() -> Result<AuthorizedKeys, AppError>,
    json: bool,
) -> Result<CommandOutput, AppError> {
    match command {
        NotifyCommand::Status => Ok(status(
            "mobile.notify.status",
            &Settings::load(path).map_err(error)?,
        )),
        NotifyCommand::Set {
            enabled,
            server,
            detail,
            finished,
            phone,
        } => {
            let server = server
                .as_deref()
                .map(notify::relay_origin)
                .transpose()
                .map_err(error)?;
            let settings = Settings::update(path, |settings| {
                if let Some(server) = server {
                    settings.server = server;
                }
                if let Some(enabled) = enabled {
                    settings.enabled = enabled;
                }
                if let Some(detail) = detail {
                    settings.detail = match detail {
                        DetailArg::Generic => Detail::Generic,
                        DetailArg::Titles => Detail::Titles,
                    };
                }
                if let Some(finished) = finished {
                    settings.finished = finished;
                }
                if let Some(phone) = phone {
                    let keys = keys().map_err(|_| "Could not find your phone pairings")?;
                    let phones = keys
                        .phones()
                        .map_err(|_| "Could not read your phone pairings")?;
                    let mut matches = phones.iter().filter(|candidate| {
                        candidate.restricted
                            && (candidate.key.fingerprint() == phone || candidate.name == phone)
                    });
                    let selected = matches
                        .next()
                        .ok_or("No paired phone matches that name or fingerprint")?;
                    if matches.any(|candidate| candidate.key != selected.key) {
                        return Err(
                            "More than one phone has that name. Use its SSH fingerprint.".into(),
                        );
                    }
                    keys.prepare_notification_phone(&selected.key)?;
                    settings.phone = Some(selected.key.fingerprint());
                }
                if settings.enabled {
                    let fingerprint = settings
                        .phone
                        .as_deref()
                        .ok_or("Choose a paired phone with --phone before enabling delivery")?;
                    keys()
                        .map_err(|_| "Could not find your phone pairings")?
                        .notification_topic(fingerprint, &settings.server)?;
                }
                Ok(())
            })
            .map_err(error)?;
            Ok(status("mobile.notify.set", &settings))
        }
        NotifyCommand::Subscribe => {
            if json || !io::stdout().is_terminal() {
                return Err(error(
                    "Show the private topic in an interactive terminal without --json",
                ));
            }
            let settings = Settings::load(path).map_err(error)?;
            let phone = settings
                .phone
                .as_deref()
                .ok_or_else(|| error("Choose a paired phone with --phone"))?;
            let topic = keys()?
                .notification_topic(phone, &settings.server)
                .map_err(error)?;
            Ok(CommandOutput {
                command: "mobile.notify.subscribe",
                data: json!({}),
                human: format!("Server  {}\nTopic   {topic}", settings.server),
            })
        }
        NotifyCommand::Test => {
            if !Settings::load(path).map_err(error)?.enabled {
                return Err(error("Enable mobile notifications before sending a test"));
            }
            match control_daemon_client()?
                .send_test_mobile_notification()
                .map_err(error)?
            {
                TestOutcome::Sent => Ok(CommandOutput {
                    command: "mobile.notify.test",
                    data: json!({"accepted": true}),
                    human: "The relay accepted the test notification".into(),
                }),
                TestOutcome::Unavailable(message) => Err(error(message)),
                TestOutcome::TimedOut => Err(error("The mobile notification test timed out")),
            }
        }
    }
}

fn status(command: &'static str, settings: &Settings) -> CommandOutput {
    let enabled = if settings.enabled { "on" } else { "off" };
    let detail = match settings.detail {
        Detail::Generic => "generic",
        Detail::Titles => "titles",
    };
    CommandOutput {
        command,
        data: json!({
            "enabled": settings.enabled,
            "server": settings.server,
            "detail": detail,
            "finished": settings.finished,
            "phone_selected": settings.phone.is_some(),
        }),
        human: format!(
            "Mobile notifications  {enabled}\nServer                {}\nDetail                {detail}\nSuccessful runs       {}\nPhone selected        {}",
            settings.server,
            if settings.finished { "on" } else { "off" },
            if settings.phone.is_some() {
                "yes"
            } else {
                "no"
            },
        ),
    }
}

fn error(error: impl std::fmt::Display) -> AppError {
    AppError::new("mobile_notification", error)
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;
    use crate::{
        control::{Cli, TopCommand},
        mobile::pairing::PublicKey,
    };

    #[test]
    fn settings_and_status_keep_pairing_secrets_out_of_cli_output() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("mobile-notify.json");
        let keys = AuthorizedKeys::in_home(root.path());
        let cli = |args: &[&str]| {
            let command = Cli::try_parse_from(
                ["cagents", "--json", "mobile", "notify"]
                    .into_iter()
                    .chain(args.iter().copied()),
            )
            .unwrap()
            .command;
            let TopCommand::Mobile { command } = command else {
                panic!("mobile command")
            };
            execute_on(command, &path, || Ok(keys.clone()), true)
        };
        let status = cli(&["status"]).unwrap();
        assert_eq!(status.data["enabled"], false);
        assert_eq!(status.data["detail"], "generic");
        assert!(!path.exists());
        assert!(cli(&["test"]).is_err());
        assert!(cli(&["set", "--enabled", "true"]).is_err());
        assert!(cli(&["set", "--server", "https://ntfy.sh/sensitive"]).is_err());
        assert!(!path.exists());

        let key = PublicKey::ed25519(&[3; 32]);
        keys.add_phone(
            &key,
            "Fixture phone",
            "/fixture/cagents daemon bridge --mobile",
        )
        .unwrap();
        let fingerprint = key.fingerprint();
        let server = notify::relay_origin("https://ntfy.sh").unwrap();
        assert!(keys.notification_topic(&fingerprint, &server).is_err());
        let selected = cli(&[
            "set",
            "--phone",
            "Fixture phone",
            "--enabled",
            "true",
            "--detail",
            "titles",
        ])
        .unwrap();
        let topic = keys.notification_topic(&fingerprint, &server).unwrap();
        assert_eq!(selected.data["enabled"], true);
        assert_eq!(selected.data["detail"], "titles");
        assert_eq!(selected.data["phone_selected"], true);
        let saved = std::fs::read(&path).unwrap();
        let excessive_server = format!("https://{}.org", "x".repeat(5000));
        assert!(cli(&["set", "--server", &excessive_server]).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), saved);
        for secret in [&topic, &fingerprint] {
            assert!(!selected.human.contains(secret));
            assert!(!selected.data.to_string().contains(secret));
            assert!(!std::fs::read_to_string(&path).unwrap().contains(&topic));
        }
        assert!(cli(&["subscribe"]).is_err()); // --json never emits the subscription secret
        keys.remove_phone(&key).unwrap();
        assert_eq!(
            cli(&["set", "--enabled", "false"]).unwrap().data["enabled"],
            false
        );
    }
}
