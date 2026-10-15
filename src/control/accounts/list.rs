//! A metadata-only projection of the existing selected-daemon AccountInfo read.
use super::*;
use crate::{account::info::AccountInfo, agent::Provider};

pub(super) fn query(provider: Option<Provider>) -> Query {
    Query { provider, account: None, refresh: false }
}

#[derive(Serialize)]
struct Entry<'a> {
    id: &'a crate::account::AccountId,
    provider: &'static str,
    name: Option<&'a str>,
    default: bool,
    current: bool,
    archived: bool,
    identity_conflict: bool,
}

fn entry(account: &AccountInfo) -> Entry<'_> {
    Entry {
        id: &account.id,
        provider: account.provider.executable(),
        name: account.label.as_deref()
            .or_else(|| account.registered_identity.as_ref().and_then(|identity| identity.email.as_deref()))
            .or_else(|| account.observed_identity.as_ref().and_then(|identity| identity.email.as_deref())),
        default: account.default_for_new_chats,
        current: account.selected_by_caller,
        archived: account.archived,
        identity_conflict: account.identity_conflict,
    }
}

pub(super) fn output(report: &Report) -> CommandOutput {
    let accounts = report.accounts.iter().map(entry).collect::<Vec<_>>();
    let yes_no = |value| if value { "yes".to_owned() } else { "no".to_owned() };
    let mut human = super::super::format::table(
        ["ACCOUNT", "PROVIDER", "NAME", "DEFAULT", "CURRENT", "ARCHIVED"],
        accounts.iter().map(|account| [
            account.id.to_string(), account.provider.into(),
            format!("{}{}", account.name.unwrap_or("identity unknown"), if account.identity_conflict { " (identity changed)" } else { "" }),
            yes_no(account.default), yes_no(account.current), yes_no(account.archived),
        ]).collect(),
        "No accounts",
    );
    if !report.diagnostics.is_empty() {
        human.push_str("\n\nAccount metadata notes:\n");
        human.push_str(&report.diagnostics.iter().map(|note| super::super::format::plain(note)).collect::<Vec<_>>().join("\n"));
    }
    CommandOutput {
        command: "account.list",
        data: json!({"accounts": accounts, "sampled_at": report.sampled_at, "diagnostics": report.diagnostics}),
        human,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_list_parses_provider_and_cannot_request_a_probe() {
        let parsed = Cli::try_parse_from(["cagents", "account", "list", "--provider", "codex", "--json"]).unwrap();
        assert!(parsed.json);
        let TopCommand::Account { command: AccountCommand::List { provider } } = parsed.command else { panic!("account list") };
        assert_eq!(query(provider), Query { provider: Some(Provider::Codex), account: None, refresh: false });
        assert!(Cli::try_parse_from(["cagents", "account", "list", "--refresh"]).is_err());
        assert!(Cli::try_parse_from(["cagents", "account", "list", "--provider", "unknown"]).is_err());
    }

    #[test]
    fn account_list_has_an_explicit_empty_state_and_keeps_metadata_notes() {
        let report = Report { sampled_at: 42, caller: None, accounts: vec![], diagnostics: vec!["Retained account metadata is unavailable".into()] };
        let output = output(&report);
        assert!(output.human.starts_with("No accounts\n\nAccount metadata notes:"));
        assert_eq!(output.data, json!({"accounts": [], "sampled_at": 42, "diagnostics": report.diagnostics}));
    }

    #[test]
    fn account_list_preserves_default_and_archived_identity_without_usage_or_recipes() {
        let account = AccountInfo {
            id: crate::account::Account::system(Provider::Claude).id(), provider: Provider::Claude,
            label: Some("My account".into()), registered_identity: None, observed_identity: None,
            identity_conflict: true, selected_by_caller: true, default_for_new_chats: true,
            archived: true, usage: None, remaining: vec![], credits: None,
            credits_remaining_fraction: None, credits_remaining: None, age_ms: None, stale: true,
            detail: Some("plan detail is not an account listing field".into()),
        };
        let output = output(&Report { sampled_at: 123, caller: None, accounts: vec![account], diagnostics: vec![] });
        assert_eq!(output.data["accounts"][0], json!({
            "id": "claude/system", "provider": "claude", "name": "My account",
            "default": true, "current": true, "archived": true, "identity_conflict": true,
        }));
        assert!(output.human.starts_with("ACCOUNT"));
        assert!(output.human.contains("My account (identity changed)"));
        assert!(!output.human.contains("plan detail"));
        assert!(output.data.get("caller").is_none());
        assert!(output.data["accounts"][0].get("usage").is_none());
        assert!(output.data["accounts"][0].get("environment").is_none());
    }
}
