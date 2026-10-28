//! A metadata-only projection of the existing selected-daemon AccountInfo read.
use super::*;
use crate::{account::info::AccountInfo, agent::Provider};

pub(super) fn query(provider: Option<Provider>) -> Query {
    Query {
        provider,
        account: None,
        refresh: false,
    }
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
        name: account
            .label
            .as_deref()
            .or_else(|| {
                account
                    .registered_identity
                    .as_ref()
                    .and_then(|identity| identity.email.as_deref())
            })
            .or_else(|| {
                account
                    .observed_identity
                    .as_ref()
                    .and_then(|identity| identity.email.as_deref())
            }),
        default: account.default_for_new_chats,
        current: account.selected_by_caller,
        archived: account.archived,
        identity_conflict: account.identity_conflict,
    }
}

pub(super) fn output(report: &Report) -> CommandOutput {
    let accounts = report.accounts.iter().map(entry).collect::<Vec<_>>();
    let yes_no = |value| {
        if value {
            "yes".to_owned()
        } else {
            "no".to_owned()
        }
    };
    let mut human = super::super::format::table(
        [
            "ACCOUNT", "PROVIDER", "NAME", "DEFAULT", "CURRENT", "ARCHIVED",
        ],
        accounts
            .iter()
            .map(|account| {
                [
                    account.id.to_string(),
                    account.provider.into(),
                    format!(
                        "{}{}",
                        account.name.unwrap_or("identity unknown"),
                        if account.identity_conflict {
                            " (identity changed)"
                        } else {
                            ""
                        }
                    ),
                    yes_no(account.default),
                    yes_no(account.current),
                    yes_no(account.archived),
                ]
            })
            .collect(),
        "No accounts",
    );
    if !report.diagnostics.is_empty() {
        human.push_str("\n\nAccount metadata notes:\n");
        human.push_str(
            &report
                .diagnostics
                .iter()
                .map(|note| super::super::format::plain(note))
                .collect::<Vec<_>>()
                .join("\n"),
        );
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
    fn account_list_uses_the_existing_framed_account_read_without_refresh_or_extra_requests() {
        use std::{os::unix::net::UnixStream, thread};
        let (client_stream, mut server_stream) = UnixStream::pair().unwrap();
        server_stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        server_stream
            .set_write_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        // A private scripted peer exercises the actual codec/caller. It grants
        // no host account authority and never starts a provider.
        let server = thread::spawn(move || {
            let proof = HostSessionProof {
                host_session_id: crate::model::HostSessionId::new("account-list-peer"),
                boot_id: crate::model::BootId::new("private-boot"),
                host_incarnation: crate::model::HostIncarnation::new("private-incarnation"),
            };
            let connect: ClientEnvelope = read_frame(&mut server_stream).unwrap().unwrap();
            assert!(matches!(connect.request, ClientRequest::Connect { .. }));
            write_frame(
                &mut server_stream,
                &ServerEnvelope {
                    version: PROTOCOL_VERSION,
                    request_id: Some(connect.request_id),
                    proof: proof.clone(),
                    message: ServerMessage::Response {
                        response: ServerResponse::Connected {
                            proof: proof.clone(),
                            resumed: vec![],
                        },
                    },
                },
            )
            .unwrap();
            let request: ClientEnvelope = read_frame(&mut server_stream).unwrap().unwrap();
            assert_eq!(request.proof.as_ref(), Some(&proof));
            assert!(matches!(
                request.request,
                ClientRequest::Agent {
                    operation: crate::agent_execution::Operation::AccountInfo(Query {
                        provider: Some(Provider::Codex),
                        account: None,
                        refresh: false,
                    }),
                    ..
                }
            ));
            let report = Report {
                sampled_at: 456,
                caller: None,
                accounts: vec![],
                diagnostics: vec![],
            };
            write_frame(
                &mut server_stream,
                &ServerEnvelope {
                    version: PROTOCOL_VERSION,
                    request_id: Some(request.request_id),
                    proof,
                    message: ServerMessage::Response {
                        response: ServerResponse::Agent(
                            crate::agent_execution::Reply::AccountInfo(report),
                        ),
                    },
                },
            )
            .unwrap();
            assert!(
                read_frame::<ClientEnvelope>(&mut server_stream)
                    .unwrap()
                    .is_none(),
                "metadata read must not issue another operation"
            );
        });
        let result = ControlClient::from_stream(client_stream)
            .map_err(protocol_error)
            .and_then(|mut client| {
                super::read_report(
                    &mut client,
                    Authority::default(),
                    query(Some(Provider::Codex)),
                )
            });
        // The client is dropped before joining, including on any returned error.
        let settled = server.join();
        settled.unwrap();
        let report = result.unwrap();
        assert_eq!(output(&report).data["sampled_at"], 456);
    }

    #[test]
    fn account_list_parses_provider_and_cannot_request_a_probe() {
        let parsed = Cli::try_parse_from([
            "cagents",
            "account",
            "list",
            "--provider",
            "codex",
            "--json",
        ])
        .unwrap();
        assert!(parsed.json);
        let TopCommand::Account {
            command: AccountCommand::List { provider },
        } = parsed.command
        else {
            panic!("account list")
        };
        assert_eq!(
            query(provider),
            Query {
                provider: Some(Provider::Codex),
                account: None,
                refresh: false
            }
        );
        assert!(Cli::try_parse_from(["cagents", "account", "list", "--refresh"]).is_err());
        assert!(
            Cli::try_parse_from(["cagents", "account", "list", "--provider", "unknown"]).is_err()
        );
    }

    #[test]
    fn account_list_has_an_explicit_empty_state_and_keeps_metadata_notes() {
        let report = Report {
            sampled_at: 42,
            caller: None,
            accounts: vec![],
            diagnostics: vec!["Retained account metadata is unavailable".into()],
        };
        let output = output(&report);
        assert!(
            output
                .human
                .starts_with("No accounts\n\nAccount metadata notes:")
        );
        assert_eq!(
            output.data,
            json!({"accounts": [], "sampled_at": 42, "diagnostics": report.diagnostics})
        );
    }

    #[test]
    fn account_list_preserves_default_and_archived_identity_without_usage_or_recipes() {
        let account = AccountInfo {
            id: crate::account::Account::system(Provider::Claude).id(),
            provider: Provider::Claude,
            label: Some("My account".into()),
            registered_identity: None,
            observed_identity: None,
            identity_conflict: true,
            selected_by_caller: true,
            default_for_new_chats: true,
            archived: true,
            usage: None,
            remaining: vec![],
            credits: None,
            credits_remaining_fraction: None,
            credits_remaining: None,
            age_ms: None,
            stale: true,
            detail: Some("plan detail is not an account listing field".into()),
        };
        let output = output(&Report {
            sampled_at: 123,
            caller: None,
            accounts: vec![account],
            diagnostics: vec![],
        });
        assert_eq!(
            output.data["accounts"][0],
            json!({
                "id": "claude/system", "provider": "claude", "name": "My account",
                "default": true, "current": true, "archived": true, "identity_conflict": true,
            })
        );
        assert!(output.human.starts_with("ACCOUNT"));
        assert!(output.human.contains("My account (identity changed)"));
        assert!(!output.human.contains("plan detail"));
        assert!(output.data.get("caller").is_none());
        assert!(output.data["accounts"][0].get("usage").is_none());
        assert!(output.data["accounts"][0].get("environment").is_none());
    }
}
