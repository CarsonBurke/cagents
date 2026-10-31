use super::*;
use crate::account::info::{Query, Report};

mod check;
mod coding_defaults;
mod import;
mod link;
mod list;
mod preferences;
mod removal;

#[derive(Debug, clap::Subcommand)]
pub enum AccountCommand {
    /// Inspect or explicitly import one staged Codex file cache; never run a task.
    Import(import::Args),
    /// Observe one Codex account's default policy and explicitly requested capabilities.
    Check(check::Args),
    /// Create absent coding defaults for one registered Codex account; preserves existing files.
    CreateCodingDefaults(coding_defaults::Args),
    /// Register an existing provider home on this device, or on --device.
    /// Does not transfer credentials or verify coding access.
    Link {
        /// Provider whose existing home is registered: claude or codex.
        #[arg(value_parser = link::parse_provider)]
        provider: crate::agent::Provider,
        /// Absolute home on the selected device, or quoted '~/path'.
        #[arg(value_parser = link::parse_home)]
        home: String,
    },
    /// Choose the selected daemon's account for new conversations.
    Default {
        /// Provider whose default changes: claude, codex, or omp (automatic only).
        #[arg(value_parser = crate::control::agents::parse_provider)]
        provider: crate::agent::Provider,
        /// A full ID from account list (for example claude/system), or automatic.
        selection: crate::account::preferences::DefaultSelection,
    },
    /// Forget a linked account registration; managed file deletion remains unavailable.
    Remove {
        /// Stable account ID from account list on the selected device.
        account: crate::account::AccountId,
        /// Confirm removing the registration; linked files are left unchanged.
        #[arg(long, required = true)]
        yes: bool,
    },
    /// List active accounts and any retained caller account from the selected daemon.
    List {
        /// Limit rows to claude, codex, or omp; never probe its CLI.
        #[arg(long, value_parser = crate::control::agents::parse_provider)]
        provider: Option<crate::agent::Provider>,
    },
    /// Read caller context and configured account plan allowances without probing.
    Info {
        /// Limit account rows to claude, codex, or omp.
        #[arg(long, value_parser = crate::control::agents::parse_provider)]
        provider: Option<crate::agent::Provider>,
        /// Select a stable provider/account ID from this report.
        #[arg(long)]
        account: Option<crate::account::AccountId>,
        /// Ask this account's provider CLI for fresh usage. Requires --account.
        #[arg(long, requires = "account")]
        refresh: bool,
    },
}

/// Run before device selection, staged-owner routing or daemon preparation.
pub(super) fn preflight(command: &AccountCommand) -> Result<(), AppError> {
    if matches!(command, AccountCommand::Import(_)) {
        import::require_user()?;
    }
    Ok(())
}

pub(super) fn execute(command: AccountCommand) -> Result<CommandOutput, AppError> {
    let (query, listing) = match command {
        AccountCommand::Import(args) => return import::execute(args),
        AccountCommand::Check(args) => return check::execute(args),
        AccountCommand::CreateCodingDefaults(args) => return coding_defaults::execute(args),
        AccountCommand::Link { provider, home } => return link::execute(provider, home),
        AccountCommand::Default {
            provider,
            selection,
        } => return preferences::execute(provider, selection),
        AccountCommand::Remove { account, yes } => return removal::execute(account, yes),
        AccountCommand::List { provider } => (list::query(provider), true),
        AccountCommand::Info {
            provider,
            account,
            refresh,
        } => (
            Query {
                provider,
                account,
                refresh,
            },
            false,
        ),
    };
    let socket = control_daemon_socket()?;
    let mut client =
        ControlClient::connect(&socket).map_err(|e| AppError::new("daemon_connection", e))?;
    let report = read_report(&mut client, Authority::from_environment(), query)?;
    if listing {
        return Ok(list::output(&report));
    }
    Ok(CommandOutput {
        command: "account.info",
        human: human(&report),
        data: serde_json::to_value(report).map_err(|e| AppError::new("account_info", e))?,
    })
}

fn read_report(
    client: &mut ControlClient,
    authority: Authority,
    query: Query,
) -> Result<Report, AppError> {
    let deadline = query
        .refresh
        .then(|| Instant::now() + Duration::from_secs(90));
    let report = match client
        .agent_before(
            authority,
            crate::agent_execution::Operation::AccountInfo(query),
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
    Ok(report)
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
                "  {:?}; plan {}; source {:?}, observed {}{}",
                usage.status,
                usage.plan.as_deref().unwrap_or("unknown"),
                usage.source,
                usage.observed_at,
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
                        .map(|at| at.to_string())
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
