use super::*;
use crate::account::provider_install::{ProviderStatus, ProviderUpdateResult};

#[derive(Debug, Subcommand)]
pub enum ProviderCommand {
    /// Show installed versions, install methods and the last update result.
    Status,
    /// Check official channels and update eligible CLI installs now.
    Update {
        /// Update only claude, codex, agy, muse, or omp. Omit to check all.
        #[arg(value_parser = agents::parse_provider, value_name = "NAME")]
        provider: Option<crate::agent::Provider>,
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
            let providers = client.update_providers(provider).map_err(protocol_error)?;
            let outcome = if providers
                .iter()
                .any(|status| status.last_result == Some(ProviderUpdateResult::Failed))
            {
                "failed"
            } else if providers
                .iter()
                .any(|status| status.last_result == Some(ProviderUpdateResult::Deferred))
            {
                "pending"
            } else {
                "succeeded"
            };
            let mut report = output("provider.update", &providers)?;
            report.data["outcome"] = json!(outcome);
            Ok(report)
        }
    }
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
