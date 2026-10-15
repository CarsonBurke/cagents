use super::*;
use crate::account::info::{Query, Report};

#[derive(Debug, clap::Subcommand)]
pub enum AccountCommand {
    /// List registered accounts and the selected defaults.
    List,
    /// Select a provider's default account, or automatic routing.
    Default {
        /// Provider whose default should change.
        #[arg(value_enum, ignore_case = true)]
        provider: crate::agent::Provider,
        /// Stable provider/account ID, system, UUID, or automatic.
        account: String,
    },
    /// Run the provider's official sign-in flow in an isolated account home.
    SignIn {
        /// Provider to sign in to; Claude and Codex manage separate homes.
        #[arg(value_enum, ignore_case = true)]
        provider: crate::agent::Provider,
    },
    /// Archive a managed account or unregister a linked account using app safety rules.
    Remove {
        /// Stable provider/account ID to remove.
        account: crate::account::AccountId,
    },
    /// Read caller context and configured account plan allowances without probing.
    Info {
        /// Limit account rows to this provider.
        #[arg(long, value_enum, ignore_case = true)]
        provider: Option<crate::agent::Provider>,
        /// Select a stable provider/account ID from this report.
        #[arg(long)]
        account: Option<crate::account::AccountId>,
        /// Ask this account's provider CLI for fresh usage. Requires --account.
        #[arg(long, requires = "account")]
        refresh: bool,
    },
}

pub(super) fn execute(command: AccountCommand) -> Result<CommandOutput, AppError> {
    let (provider, account, refresh) = match command {
        AccountCommand::Info { provider, account, refresh } => (provider, account, refresh),
        command => return management(command),
    };
    let socket = control_daemon_socket()?;
    let mut client =
        ControlClient::connect(&socket).map_err(|e| AppError::new("daemon_connection", e))?;
    let deadline = refresh.then(|| Instant::now() + Duration::from_secs(90));
    let mut report = match client
        .agent_before(
            Authority::from_environment(),
            crate::agent_execution::Operation::AccountInfo(Query {
                provider,
                account,
                refresh,
            }),
            deadline,
        )
        .map_err(protocol_error)?
    {
        crate::agent_execution::Reply::AccountInfo(report) => report,
        _ => {
            return Err(AppError::new(
                "account_info",
                "Unexpected account usage response",
            ));
        }
    };
    let installed = snapshot(&mut client)?;
    report.accounts.retain(|account| !account.id.as_str().ends_with("/system") || installed["installed"].as_object()
        .is_some_and(|providers| providers.iter().any(|(provider, value)| provider.eq_ignore_ascii_case(account.id.as_str().split('/').next().unwrap_or_default()) && value == true)));
    if installed["installed"].as_object().is_none_or(|providers| !providers.iter().any(|(provider, value)| provider.eq_ignore_ascii_case("omp") && value == true)) {
        report.diagnostics.retain(|line| !line.starts_with("OMP has no recorded"));
    }
    Ok(CommandOutput {
        command: "account.info",
        human: human(&report),
        data: serde_json::to_value(report).map_err(|e| AppError::new("account_info", e))?,
    })
}

fn snapshot(client: &mut ControlClient) -> Result<Value, AppError> {
    match client.device_operation(crate::device_service::Operation::AccountsRead).map_err(protocol_error)? {
        crate::device_service::Reply::Accounts(value) => Ok(value),
        _ => Err(AppError::new("account", "Unexpected account registry response")),
    }
}

fn lifecycle(client: &mut ControlClient, operation: crate::account::remote::Operation) -> Result<Value, AppError> {
    match client.device_operation(crate::device_service::Operation::AccountLifecycle { operation }).map_err(protocol_error)? {
        crate::device_service::Reply::AccountLifecycle(value) => Ok(value),
        _ => Err(AppError::new("account", "Unexpected account lifecycle response")),
    }
}

fn management(command: AccountCommand) -> Result<CommandOutput, AppError> {
    use crate::{account::remote::Operation, agent::Provider};
    if matches!(command, AccountCommand::Remove { .. }) && Authority::from_environment().token().is_some() {
        return Err(AppError::new("account_authority", "Account removal is refused under an agent capability; run it from your own shell"));
    }
    let socket = control_daemon_socket()?;
    let mut client = ControlClient::connect(&socket).map_err(protocol_error)?;
    match command {
        AccountCommand::List => {
            let snapshot = snapshot(&mut client)?;
            let accounts = account_rows(&snapshot);
            let human = format::table(&["ID", "LABEL", "DEFAULT", "STATUS"], accounts.iter().map(|account| vec![
                account["id"].as_str().unwrap_or_default().into(), account["label"].as_str().unwrap_or_default().into(),
                if account["default"] == true { "selected".into() } else { String::new() },
                if account["archived"] == true { "archived" } else if account["installed"] == false { "provider not installed" } else { "available" }.into(),
            ]).collect(), "No configured accounts");
            Ok(CommandOutput { command: "account.list", data: json!({"accounts": accounts, "revision": snapshot["registry"]["revision"]}), human })
        }
        AccountCommand::Default { provider, account } => {
            if provider == Provider::Omp { return Err(AppError::new("account", "OMP manages its own account selection")); }
            let snapshot = snapshot(&mut client)?;
            let mut registry = snapshot["registry"].clone();
            let automatic = account.eq_ignore_ascii_case("automatic");
            let selected = if automatic { None } else {
                let id = if account.contains('/') { account } else { format!("{}/{account}", provider.executable()) };
                let _: crate::account::AccountId = id.parse().map_err(|e| AppError::new("account", e))?;
                if !id.starts_with(&format!("{}/", provider.executable())) || !account_rows(&snapshot).iter().any(|row| row["id"] == id && row["archived"] != true) {
                    return Err(AppError::new("account", "Choose an available account of this provider from account list"));
                }
                Some(id)
            };
            let object = registry.as_object_mut().ok_or_else(|| AppError::new("account", "Invalid account registry"))?;
            let defaults = object.entry("defaults").or_insert_with(|| json!({})).as_object_mut().ok_or_else(|| AppError::new("account", "Invalid account defaults"))?;
            if let Some(id) = &selected { defaults.insert(provider.executable().into(), json!(id.split_once('/').unwrap().1)); }
            else { defaults.remove(provider.executable()); }
            let expected_revision = snapshot["registry"]["revision"].as_u64().unwrap_or_default();
            let revision = match client.device_operation(crate::device_service::Operation::AccountsUpdate { expected_revision, registry }).map_err(protocol_error)? {
                crate::device_service::Reply::AccountRegistry(registry) => registry["revision"].clone(),
                _ => return Err(AppError::new("account", "Unexpected account preference response")),
            };
            Ok(CommandOutput { command: "account.default", data: json!({"provider": provider.executable(), "account": selected, "automatic": automatic, "revision": revision}),
                human: format!("{} default: {}", provider.executable(), selected.as_deref().unwrap_or("automatic")) })
        }
        AccountCommand::Remove { account } => {
            lifecycle(&mut client, Operation::Remove { id: account.clone() })?;
            Ok(CommandOutput { command: "account.remove", data: json!({"account": account, "state": "removed", "transcript_files_retained": true}),
                human: format!("Removed {account}; managed transcript files are retained") })
        }
        AccountCommand::SignIn { provider } => {
            let created = lifecycle(&mut client, Operation::Create { provider, copy_codex_config: false })?;
            let key: uuid::Uuid = serde_json::from_value(created["key"].clone()).map_err(|e| AppError::new("account", e))?;
            let recipe: LaunchRecipe = serde_json::from_value(created["recipe"].clone()).map_err(|e| AppError::new("account", e))?;
            let result = run_login(&recipe);
            if !result.as_ref().is_ok_and(|status| status.success()) {
                let _ = lifecycle(&mut client, Operation::Abandon { provider, key });
                return Err(AppError::new("account_sign_in", match result { Ok(status) => format!("Provider sign-in exited {status}"), Err(error) => error.to_string() }));
            }
            let settled = lifecycle(&mut client, Operation::Register { provider, key })?;
            if let Some(duplicate) = settled.get("Duplicate") {
                return Ok(CommandOutput { command: "account.sign_in", data: json!({"account": duplicate["existing"], "state": "already_registered"}), human: "This login already has a registered account".into() });
            }
            if settled.get("Abandoned").is_some() {
                return Err(AppError::new("account_sign_in", "The provider did not confirm a login; the unused sign-in home was removed"));
            }
            if settled.get("Registered").is_none() {
                return Err(AppError::new("account_sign_in", "Unexpected sign-in verification response"));
            }
            let account = format!("{}/{key}", provider.executable());
            Ok(CommandOutput { command: "account.sign_in", data: json!({"account": account, "state": "signed_in"}), human: format!("Signed in as {account}") })
        }
        AccountCommand::Info { .. } => unreachable!(),
    }
}

fn account_rows(snapshot: &Value) -> Vec<Value> {
    let registry = &snapshot["registry"];
    let installed = |provider: &str| snapshot["installed"].as_object().and_then(|entries| entries.iter().find(|(name, _)| name.eq_ignore_ascii_case(provider))).and_then(|(_, installed)| installed.as_bool()).unwrap_or(false);
    let mut rows = Vec::new();
    for provider in ["claude", "codex"] {
        if installed(provider) {
            rows.push(json!({"id": format!("{provider}/system"), "provider": provider, "label": "System account",
                "default": registry["defaults"][provider].is_null() || registry["defaults"][provider] == "system", "installed": true, "archived": false}));
        }
    }
    for account in registry["account"].as_array().into_iter().flatten() {
        let provider = account["provider"].as_str().unwrap_or_default().to_ascii_lowercase();
        let key = account["key"].as_str().unwrap_or_default();
        let archived = registry["archived"].as_array().is_some_and(|keys| keys.iter().any(|value| value == key));
        rows.push(json!({"id": format!("{provider}/{key}"), "provider": provider, "label": account["label"].as_str().or_else(|| account["email"].as_str()).unwrap_or("Unnamed account"),
            "default": registry["defaults"][&provider] == key, "installed": installed(&provider), "archived": archived}));
    }
    rows
}

fn run_login(recipe: &LaunchRecipe) -> io::Result<std::process::ExitStatus> {
    use std::os::fd::AsFd;
    let quote = |value: &str| format!("'{}'", value.replace('\'', "'\"'\"'"));
    let mut command = if let Some(device) = crate::device::selected() {
        let mut command = Command::new("ssh");
        if io::stdin().is_terminal() { command.arg("-t"); }
        let mut invocation = vec!["env".into()];
        for key in &recipe.unset_environment { invocation.push("-u".into()); invocation.push(quote(key)); }
        for (key, value) in &recipe.environment { invocation.push(quote(&format!("{key}={value}"))); }
        invocation.push(quote(&recipe.program)); invocation.extend(recipe.arguments.iter().map(|value| quote(value)));
        command.args(["--", &device.host, &format!("cd -- {} && exec {}", quote(&recipe.working_directory.to_string_lossy()), invocation.join(" "))]);
        command
    } else {
        let mut command = Command::new(&recipe.program);
        command.args(&recipe.arguments).current_dir(&recipe.working_directory).envs(&recipe.environment);
        for key in &recipe.unset_environment { command.env_remove(key); }
        command
    };
    command.stdin(Stdio::inherit()).stdout(Stdio::from(io::stderr().as_fd().try_clone_to_owned()?)).stderr(Stdio::inherit()).status()
}

fn human(report: &Report) -> String {
    let mut lines = Vec::new();
    if let Some(caller) = &report.caller {
        lines.push(format!(
            "Session {}  account {}",
            caller.session,
            caller
                .account
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_else(|| "unknown".into())
        ));
        lines.push(match (caller.context_tokens, caller.context_limit) {
            (Some(used), Some(limit)) => format!(
                "Context: {used}/{limit} tokens ({:.1}% used)",
                used as f64 * 100.0 / limit as f64
            ),
            (Some(used), None) => format!("Context: {used} tokens; limit unknown"),
            (None, Some(limit)) => {
                format!("Context: occupancy unknown; reported limit {limit} tokens")
            }
            _ => "Context: unavailable from this provider session".into(),
        });
        if let Some(detail) = &caller.account_detail {
            lines.push(detail.clone());
        }
    } else {
        lines.push("Caller context: not running inside a live cagents session".into());
    }
    for account in &report.accounts {
        let name = account
            .label
            .as_deref()
            .or_else(|| {
                account
                    .registered_identity
                    .as_ref()
                    .and_then(|i| i.email.as_deref())
            })
            .or_else(|| {
                account
                    .observed_identity
                    .as_ref()
                    .and_then(|i| i.email.as_deref())
            })
            .unwrap_or("identity unknown");
        lines.push(format!(
            "\n{}  {}{}{}{}",
            account.id,
            name,
            if account.selected_by_caller {
                " (current)"
            } else {
                ""
            },
            if account.archived { " (archived)" } else { "" },
            if account.identity_conflict {
                " — login identity changed"
            } else {
                ""
            }
        ));
        if let Some(usage) = &account.usage {
            lines.push(format!(
                "  {}; plan {}; source {}, observed {}{}",
                format::label(usage.status),
                usage.plan.as_deref().unwrap_or("unknown"),
                format::label(usage.source),
                format::relative_time(usage.observed_at),
                if account.stale { " (stale)" } else { "" }
            ));
            for remaining in &account.remaining {
                lines.push(format!(
                    "  {}: {:.0}% remaining; resets {}{}",
                    remaining.window.label,
                    remaining.remaining_fraction * 100.0,
                    remaining
                        .window
                        .resets_at
                        .map(|at| format::relative_time((at.max(0) as u64).saturating_mul(1000)))
                        .unwrap_or_else(|| "unknown".into()),
                    if remaining.reset_elapsed {
                        " (reset passed; refresh required)"
                    } else {
                        ""
                    }
                ));
            }
            if let Some(credits) = &account.credits {
                lines.push(format!("  Extra usage: {}", credits.summary()));
                if let Some(remaining) = &account.credits_remaining {
                    lines.push(format!(
                        "  Extra usage remaining: {remaining} (limit minus used)"
                    ));
                }
            }
        } else {
            lines.push("  Plan usage: unknown (no cached observation)".into());
        }
        if let Some(detail) = &account.detail {
            lines.push(format!("  {detail}"));
        }
    }
    lines.extend(report.diagnostics.iter().cloned());
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn account_info_defaults_to_cached_and_refresh_names_one_account() {
        let parsed = Cli::try_parse_from(["cagents", "account", "info", "--json"]).unwrap();
        assert!(parsed.json);
        assert!(matches!(
            parsed.command,
            TopCommand::Account {
                command: AccountCommand::Info {
                    refresh: false,
                    account: None,
                    provider: None
                }
            }
        ));
        assert!(Cli::try_parse_from(["cagents", "account", "info", "--refresh"]).is_err());
        assert!(
            Cli::try_parse_from([
                "cagents",
                "account",
                "info",
                "--refresh",
                "--account",
                "codex/system"
            ])
            .is_ok()
        );
        assert!(
            Cli::try_parse_from(["cagents", "account", "info", "--provider", "invalid"]).is_err()
        );
    }
    #[test]
    fn human_report_keeps_unknown_context_and_plan_distinct() {
        let report = Report {
            sampled_at: 0,
            caller: Some(crate::account::info::Caller::new(
                TerminalId::new(),
                Some(crate::agent::Provider::Codex),
                None,
                None,
                Some(Default::default()),
            )),
            accounts: vec![],
            diagnostics: vec!["OMP has no recorded account usage".into()],
        };
        let output = human(&report);
        assert!(output.contains("account unknown"));
        assert!(output.contains("Context: unavailable"));
        assert!(!output.contains("0%"));
        assert!(output.contains("OMP has no recorded"));
        let json = serde_json::to_value(report).unwrap();
        assert!(json["caller"]["context_limit"].is_null());
        assert!(json["caller"]["account"].is_null());
    }
}

#[cfg(test)]
mod management_tests {
    use super::*;
    #[test]
    fn account_management_arguments_and_snapshot_shape() {
        for args in [vec!["cagents", "account", "list"], vec!["cagents", "account", "default", "Claude", "automatic"],
            vec!["cagents", "account", "sign-in", "codex"], vec!["cagents", "account", "remove", "codex/system"]] {
            assert!(Cli::try_parse_from(args).is_ok());
        }
        let rows = account_rows(&json!({"installed":{"Claude":true,"Codex":false},"registry":{"defaults":{"claude":"system"},"account":[{"provider":"codex","key":"00000000-0000-4000-8000-000000000000","label":"Fixture"}]}}));
        assert_eq!(rows.len(), 2); assert_eq!(rows[0]["id"], "claude/system");
        assert_eq!(rows[0]["default"], true); assert_eq!(rows[1]["installed"], false);
    }
}
