use super::*;
use crate::settings::commands::Operation;

#[derive(Debug, Subcommand)]
pub enum SettingsCommand {
    /// Read a setting as a typed JSON value.
    Get {
        /// Setting name; use dots for nested keys.
        key: String,
    },
    /// Validate and save one setting; running windows reload the file.
    Set {
        /// Setting name; use dots for nested keys.
        key: String,
        /// TOML value, or an unquoted string; use null to clear an optional value.
        value: String,
    },
    /// Print the settings.toml location.
    Path,
    /// Create settings.toml if needed and open it in the default application.
    Open,
}

pub(super) fn remote_preflight(command: &SettingsCommand, remote: bool) -> Result<(), AppError> {
    if remote && matches!(command, SettingsCommand::Open) {
        return Err(AppError::new(
            "settings_remote_open",
            "Open the path returned by settings path on the selected device",
        ));
    }
    Ok(())
}

pub(super) fn execute(command: SettingsCommand) -> Result<CommandOutput, AppError> {
    let open = matches!(command, SettingsCommand::Open);
    remote_preflight(&command, crate::device::is_remote())?;
    let (name, operation) = match command {
        SettingsCommand::Get { key } => ("settings.get", Operation::Get { key }),
        SettingsCommand::Set { key, value } => ("settings.set", Operation::Set { key, value }),
        SettingsCommand::Path => ("settings.path", Operation::Path { create: false }),
        SettingsCommand::Open => ("settings.open", Operation::Path { create: true }),
    };
    let data = if crate::device::is_remote() {
        let socket = control_daemon_socket()?;
        let mut client = ControlClient::connect(&socket).map_err(protocol_error)?;
        match client.device_operation(crate::device_service::Operation::Settings { operation }).map_err(protocol_error)? {
            crate::device_service::Reply::Settings(data) => data,
            _ => return Err(AppError::new("settings", "Unexpected settings response")),
        }
    } else {
        let path = crate::settings::default_path().map_err(|e| AppError::new("settings", e))?;
        crate::settings::commands::execute(&path, operation).map_err(|e| AppError::new("settings", e))?
    };
    if open {
        let path = data["path"].as_str().ok_or_else(|| AppError::new("settings", "Missing settings path"))?;
        Command::new("xdg-open").arg(path).stdin(Stdio::null()).stdout(Stdio::null())
            .stderr(Stdio::null()).spawn().map_err(|e| AppError::new("settings_open", e))?;
    }
    let human = if let Some(key) = data["key"].as_str() {
        format!("{key} = {}", data["value"])
    } else { data["path"].as_str().unwrap_or_default().into() };
    Ok(CommandOutput { command: name, data, human })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unsupported_remote_open_is_refused_by_preflight() {
        let error = remote_preflight(&SettingsCommand::Open, true).unwrap_err();
        assert_eq!(error.kind, "settings_remote_open");
        assert_eq!(
            error.message,
            "Open the path returned by settings path on the selected device"
        );
        remote_preflight(&SettingsCommand::Open, false).unwrap();
        for command in [
            SettingsCommand::Path,
            SettingsCommand::Get {
                key: "terminal_font_size".into(),
            },
            SettingsCommand::Set {
                key: "terminal_font_size".into(),
                value: "15".into(),
            },
        ] {
            remote_preflight(&command, true).unwrap();
        }
    }

    #[test]
    fn settings_commands_parse_typed_values_and_paths() {
        for arguments in [vec!["cagents", "settings", "get", "terminal_font_size"],
            vec!["cagents", "settings", "set", "terminal_font_size", "15"],
            vec!["cagents", "settings", "path"], vec!["cagents", "settings", "open"]] {
            assert!(Cli::try_parse_from(arguments).is_ok());
        }
    }
}
