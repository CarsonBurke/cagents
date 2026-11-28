use super::*;
use crate::{
    account::provider_install::{ProviderStatus, ProviderUpdateResult},
    agent::Provider,
};

#[derive(Debug, Subcommand)]
pub enum ProviderCommand {
    /// Show installed versions, install methods and the last update result.
    Status,
    /// Check for a new release and update a managed provider CLI.
    Update {
        /// Provider to update, such as claude.
        #[arg(value_parser = agents::parse_provider, value_name = "NAME")]
        provider: Provider,
    },
    /// Install a provider CLI that the daemon can keep current.
    Manage {
        /// Provider to install under Cagents management, such as claude.
        #[arg(value_parser = agents::parse_provider, value_name = "NAME")]
        provider: Provider,
    },
}

pub(super) fn execute(command: ProviderCommand) -> Result<CommandOutput, AppError> {
    let mut client = control_daemon_client()?;
    client
        .authenticate(Authority::from_environment())
        .map_err(protocol_error)?;
    match command {
        ProviderCommand::Status => {
            let providers = client.provider_status().map_err(protocol_error)?;
            output("provider.status", &providers)
        }
        ProviderCommand::Update { provider } => {
            let providers = client.provider_update(provider).map_err(protocol_error)?;
            maintenance_output("provider.update", &providers)
        }
        ProviderCommand::Manage { provider } => {
            let providers = client.provider_manage(provider).map_err(protocol_error)?;
            maintenance_output("provider.manage", &providers)
        }
    }
}

fn maintenance_output(
    command: &'static str,
    providers: &[ProviderStatus],
) -> Result<CommandOutput, AppError> {
    let outcome = if providers
        .iter()
        .any(|status| status.last_result == Some(ProviderUpdateResult::Failed))
    {
        WaitOutcome::Failed
    } else if providers
        .iter()
        .any(|status| status.last_result == Some(ProviderUpdateResult::Deferred))
    {
        WaitOutcome::Pending
    } else {
        WaitOutcome::Succeeded
    };
    let mut report = output(command, providers)?;
    report.data["outcome"] = json!(outcome);
    Ok(report)
}

fn output(command: &'static str, providers: &[ProviderStatus]) -> Result<CommandOutput, AppError> {
    let now = crate::chat::now_ms().unwrap_or_default();
    let rows = providers
        .iter()
        .map(|status| {
            let checked = status
                .last_checked_at
                .map(|stamp| format::relative_time(stamp, now))
                .unwrap_or_else(|| "never".to_owned());
            let result = match status.last_result {
                None => "-",
                Some(ProviderUpdateResult::Updated) => "updated",
                Some(ProviderUpdateResult::Current) => "up to date",
                Some(ProviderUpdateResult::Skipped) => "skipped",
                Some(ProviderUpdateResult::Failed) => "failed",
                Some(ProviderUpdateResult::Deferred) => "deferred",
            };
            [
                status.provider.executable().to_owned(),
                status.version.as_deref().unwrap_or("-").to_owned(),
                status.install_method.label().to_owned(),
                checked,
                result.to_owned(),
                status.error.as_deref().unwrap_or("-").to_owned(),
            ]
        })
        .collect();
    let human = format::table(
        [
            "Provider",
            "Version",
            "Install method",
            "Last check",
            "Result",
            "Details",
        ],
        rows,
        "No providers found.",
    );
    let providers =
        serde_json::to_value(providers).map_err(|error| AppError::new("provider_status", error))?;
    Ok(CommandOutput {
        command,
        data: json!({"providers": providers}),
        human,
    })
}

#[cfg(all(test, not(any(feature = "host", feature = "ui"))))]
mod tests {
    use super::*;
    use crate::account::provider_install::ProviderInstallMethod;

    #[test]
    fn maintenance_commands_require_a_provider() {
        for command in ["manage", "update"] {
            assert!(Cli::try_parse_from(["cagents", "provider", command, "Claude"]).is_ok());
            assert!(Cli::try_parse_from(["cagents", "provider", command]).is_err());
            assert!(Cli::try_parse_from(["cagents", "provider", command, "unknown"]).is_err());
        }
    }

    #[test]
    fn maintenance_failures_and_deferrals_keep_status_details() {
        for (result, outcome, code) in [
            (ProviderUpdateResult::Failed, "failed", 4),
            (ProviderUpdateResult::Deferred, "pending", 3),
        ] {
            let status = ProviderStatus {
                provider: Provider::Claude,
                executable: Some(PathBuf::from("/fixture/claude")),
                resolved_executable: None,
                version: Some("1.2.3".into()),
                install_method: ProviderInstallMethod::CagentsManaged,
                last_checked_at: Some(7),
                last_result: Some(result),
                error: Some("The existing installation remains available".into()),
            };
            let report = maintenance_output("provider.update", &[status]).unwrap();
            assert_eq!(report.exit_code(), ExitCode::from(code));
            assert_eq!(report.data["outcome"], outcome);
            assert_eq!(report.data["providers"][0]["version"], "1.2.3");
            assert!(
                report
                    .human
                    .contains("The existing installation remains available")
            );
        }
    }
}
