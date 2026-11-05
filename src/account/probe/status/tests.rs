//! Pure status-parser contracts.

use super::{LoginStatus, parse_claude_output, parse_claude_status, parse_codex_status};
use crate::account::{Identity, probe::OUTPUT_LIMIT};
use serde_json::{Value, json};

fn signed_in(result: Result<LoginStatus, String>) -> Identity {
    match result {
        Ok(LoginStatus::SignedIn(identity)) => identity,
        Ok(LoginStatus::SignedOut) => panic!("expected a confirmed login, got signed out"),
        Err(error) => panic!("expected a confirmed login, got an error: {error}"),
    }
}

fn assert_signed_out(result: Result<LoginStatus, String>) {
    match result {
        Ok(LoginStatus::SignedOut) => {}
        Ok(LoginStatus::SignedIn(_)) => panic!("a non-login must not confirm an identity"),
        Err(error) => panic!("expected an explicit non-login, got an error: {error}"),
    }
}

#[test]
fn claude_status_keeps_only_the_confirmed_trimmed_identity() {
    let identity = signed_in(parse_claude_status(&json!({
        "loggedIn": true,
        "email": " \tclaude-recovery@example.invalid\n",
        "orgName": "  Fixture organization  ",
        "subscriptionType": "  pro  ",
        "ignored": "not identity metadata"
    })));
    assert_eq!(
        identity,
        Identity {
            email: Some("claude-recovery@example.invalid".into()),
            organization: Some("Fixture organization".into()),
            plan: Some("pro".into()),
        }
    );
}

#[test]
fn claude_status_does_not_require_optional_organization_or_plan() {
    for optional in [
        json!({}),
        json!({"orgName": null, "subscriptionType": null}),
        json!({"orgName": " \t ", "subscriptionType": "\n"}),
    ] {
        let mut status = json!({
            "loggedIn": true,
            "email": "claude-recovery@example.invalid"
        });
        status
            .as_object_mut()
            .unwrap()
            .extend(optional.as_object().unwrap().clone());
        let identity = signed_in(parse_claude_status(&status));
        assert_eq!(
            identity.email.as_deref(),
            Some("claude-recovery@example.invalid")
        );
        assert_eq!(identity.organization, None);
        assert_eq!(identity.plan, None);
    }
}

#[test]
fn claude_status_requires_an_explicit_boolean_login_answer() {
    for status in [
        Value::Null,
        json!([]),
        json!(true),
        json!("logged in"),
        json!({}),
        json!({"email": "claude-recovery@example.invalid"}),
        json!({"loggedIn": null, "email": "claude-recovery@example.invalid"}),
        json!({"loggedIn": "true", "email": "claude-recovery@example.invalid"}),
        json!({"loggedIn": 1, "email": "claude-recovery@example.invalid"}),
        json!({"loggedIn": [], "email": "claude-recovery@example.invalid"}),
        json!({"loggedIn": {}, "email": "claude-recovery@example.invalid"}),
    ] {
        assert!(
            parse_claude_status(&status).is_err(),
            "accepted malformed status: {status}"
        );
    }
}

#[test]
fn claude_status_requires_a_nonempty_string_email_for_a_login() {
    assert!(parse_claude_status(&json!({"loggedIn": true})).is_err());
    for email in [
        Value::Null,
        json!(""),
        json!(" \t\r\n "),
        json!(false),
        json!(42),
        json!([]),
        json!({"email": "claude-recovery@example.invalid"}),
    ] {
        let status = json!({"loggedIn": true, "email": email});
        assert!(
            parse_claude_status(&status).is_err(),
            "accepted unstable identity: {status}"
        );
    }
}

#[test]
fn claude_explicit_signed_out_overrides_retained_identity_fields() {
    for status in [
        json!({"loggedIn": false}),
        json!({
            "loggedIn": false,
            "email": "retained@example.invalid",
            "orgName": "Retained organization",
            "subscriptionType": "pro"
        }),
        json!({"loggedIn": false, "email": {"malformed": true}}),
    ] {
        assert_signed_out(parse_claude_status(&status));
    }
}

#[test]
fn codex_status_keeps_the_confirmed_trimmed_chatgpt_identity() {
    let identity = signed_in(parse_codex_status(&json!({
        "account": {
            "type": "chatgpt",
            "email": " \tcodex-recovery@example.invalid\n",
            "planType": "  plus  ",
            "organization": "not an account/read identity field"
        },
        "requiresOpenaiAuth": true
    })));
    assert_eq!(
        identity,
        Identity {
            email: Some("codex-recovery@example.invalid".into()),
            organization: None,
            plan: Some("plus".into()),
        }
    );
}

#[test]
fn codex_chatgpt_identity_does_not_require_optional_plan_or_auth_flag() {
    for plan in [
        None,
        Some(Value::Null),
        Some(json!(" \t ")),
        Some(json!("unknown")),
    ] {
        let mut account = json!({
            "type": "chatgpt",
            "email": "codex-recovery@example.invalid"
        });
        if let Some(plan) = plan {
            account
                .as_object_mut()
                .unwrap()
                .insert("planType".into(), plan);
        }
        let identity = signed_in(parse_codex_status(&json!({"account": account})));
        assert_eq!(
            identity.email.as_deref(),
            Some("codex-recovery@example.invalid")
        );
        assert_eq!(identity.organization, None);
        assert_eq!(identity.plan, None);
    }
}

#[test]
fn codex_null_account_requires_a_boolean_auth_answer() {
    for requires_auth in [false, true] {
        assert_signed_out(parse_codex_status(&json!({
            "account": null,
            "requiresOpenaiAuth": requires_auth
        })));
    }
    assert!(parse_codex_status(&json!({"account": null})).is_err());
    for requires_auth in [Value::Null, json!("false"), json!(0), json!([]), json!({})] {
        let status = json!({"account": null, "requiresOpenaiAuth": requires_auth});
        assert!(
            parse_codex_status(&status).is_err(),
            "accepted malformed auth answer: {status}"
        );
    }
}

#[test]
fn codex_api_key_account_never_confirms_a_managed_chatgpt_login() {
    for status in [
        json!({"account": {"type": "apiKey"}}),
        json!({
            "account": {
                "type": "apiKey",
                "email": "retained@example.invalid",
                "planType": "plus"
            },
            "requiresOpenaiAuth": true
        }),
    ] {
        assert_signed_out(parse_codex_status(&status));
    }
}

#[test]
fn codex_status_requires_an_explicit_account_value() {
    for status in [
        Value::Null,
        json!([]),
        json!(false),
        json!("signed in"),
        json!({}),
        json!({"requiresOpenaiAuth": true}),
        json!({"requiresOpenaiAuth": false}),
        json!({"account": false}),
        json!({"account": 42}),
        json!({"account": "chatgpt"}),
        json!({"account": []}),
    ] {
        assert!(
            parse_codex_status(&status).is_err(),
            "accepted malformed account answer: {status}"
        );
    }
}

#[test]
fn codex_status_rejects_unknown_or_malformed_account_types() {
    assert!(
        parse_codex_status(&json!({
            "account": {"email": "codex-recovery@example.invalid"}
        }))
        .is_err()
    );
    for account_type in [
        Value::Null,
        json!(""),
        json!("unknown"),
        json!("ChatGPT"),
        json!(" chatgpt "),
        json!(true),
        json!(1),
        json!([]),
        json!({}),
    ] {
        let status = json!({
            "account": {"type": account_type, "email": "codex-recovery@example.invalid"}
        });
        assert!(
            parse_codex_status(&status).is_err(),
            "accepted unknown account type: {status}"
        );
    }
}

#[test]
fn codex_chatgpt_status_requires_a_nonempty_string_email() {
    assert!(parse_codex_status(&json!({"account": {"type": "chatgpt"}})).is_err());
    for email in [
        Value::Null,
        json!(""),
        json!(" \t\r\n "),
        json!(false),
        json!(42),
        json!([]),
        json!({"email": "codex-recovery@example.invalid"}),
    ] {
        let status = json!({"account": {"type": "chatgpt", "email": email}});
        assert!(
            parse_codex_status(&status).is_err(),
            "accepted unstable identity: {status}"
        );
    }
}

#[test]
fn status_identity_confirmation_has_no_quota_or_model_dependency() {
    let claude = signed_in(parse_claude_status(&json!({
        "loggedIn": true,
        "email": "claude-recovery@example.invalid"
    })));
    let codex = signed_in(parse_codex_status(&json!({
        "account": {"type": "chatgpt", "email": "codex-recovery@example.invalid"}
    })));
    assert_eq!(
        claude.email.as_deref(),
        Some("claude-recovery@example.invalid")
    );
    assert_eq!(
        codex.email.as_deref(),
        Some("codex-recovery@example.invalid")
    );
}

#[test]
fn claude_output_accepts_successful_json_and_explicit_signed_out_only() {
    let identity = signed_in(parse_claude_output(
        b" \n{\"loggedIn\":true,\"email\":\"claude-recovery@example.invalid\"}\t\n",
        Some(0),
    ));
    assert_eq!(
        identity.email.as_deref(),
        Some("claude-recovery@example.invalid")
    );
    assert_signed_out(parse_claude_output(b"{\"loggedIn\":false}", Some(0)));
    assert_signed_out(parse_claude_output(b"{\"loggedIn\":false}", Some(1)));
}

#[test]
fn unsuccessful_claude_exit_cannot_confirm_a_login() {
    for output in [
        b"{\"loggedIn\":true,\"email\":\"claude-recovery@example.invalid\"}".as_slice(),
        b"{\"loggedIn\":false}".as_slice(),
        b"{}".as_slice(),
        b"".as_slice(),
    ] {
        for exit_code in [Some(23), None] {
            assert!(parse_claude_output(output, exit_code).is_err());
        }
    }
    assert!(
        parse_claude_output(
            b"{\"loggedIn\":true,\"email\":\"claude-recovery@example.invalid\"}",
            Some(1)
        )
        .is_err()
    );
    assert!(parse_claude_output(b"{}", Some(1)).is_err());
}

#[test]
fn claude_output_rejects_malformed_json_and_invalid_status_shapes() {
    for output in [
        b"".as_slice(),
        b" \t\n".as_slice(),
        b"\xff".as_slice(),
        b"{\"loggedIn\":true,".as_slice(),
        b"{\"loggedIn\":false} trailing".as_slice(),
        b"{\"loggedIn\":false}{\"loggedIn\":false}".as_slice(),
        b"null".as_slice(),
        b"[]".as_slice(),
        b"{}".as_slice(),
        b"{\"loggedIn\":\"false\"}".as_slice(),
        b"{\"loggedIn\":true,\"email\":\" \t \"}".as_slice(),
    ] {
        assert!(parse_claude_output(output, Some(0)).is_err());
    }
}

#[test]
fn claude_output_accepts_exactly_the_limit_but_rejects_a_valid_prefix_over_it() {
    let mut output = b"{\"loggedIn\":true,\"email\":\"claude-recovery@example.invalid\"}".to_vec();
    output.resize(usize::try_from(OUTPUT_LIMIT).unwrap(), b' ');
    let identity = signed_in(parse_claude_output(&output, Some(0)));
    assert_eq!(
        identity.email.as_deref(),
        Some("claude-recovery@example.invalid")
    );

    // The extra byte is valid JSON whitespace. Reject it because the actual
    // output exceeded the bound, rather than accepting its truncated prefix.
    output.push(b' ');
    assert!(parse_claude_output(&output, Some(0)).is_err());
}

#[test]
fn provider_identity_rejects_control_characters_inside_the_address() {
    for email in [
        "first\nsecond@example.invalid",
        "first\0second@example.invalid",
    ] {
        assert!(parse_claude_status(&json!({"loggedIn": true, "email": email})).is_err());
        assert!(
            parse_codex_status(&json!({
                "account": {"type": "chatgpt", "email": email}
            }))
            .is_err()
        );
    }
}

#[test]
fn prepared_status_commands_keep_the_selected_executable_and_path_for_both_subjects() {
    use std::{ffi::OsStr, path::Path};

    use crate::{account::AccountHome, agent::Provider};

    let selected = std::ffi::OsString::from(
        "/cagents-fixture/selected-runtime/bin:/cagents-fixture/selected-cli/bin",
    );
    let refreshed =
        std::ffi::OsString::from("/cagents-fixture/new-runtime/bin:/cagents-fixture/new-cli/bin");
    for provider in [Provider::Claude, Provider::Codex] {
        let program = Path::new("/cagents-fixture/selected-cli/bin").join(provider.executable());
        let home = Path::new("/cagents-fixture/reserved-home");
        for account in [
            crate::account::Account::managed(provider, uuid::Uuid::from_u128(7), home.into()),
            crate::account::Account {
                provider,
                home: AccountHome::SystemAt { path: home.into() },
            },
        ] {
            let command =
                super::status_command_with_path(&account, &program, Some(&selected)).unwrap();
            let newer =
                super::status_command_with_path(&account, &program, Some(&refreshed)).unwrap();
            let path = |command: &tokio::process::Command| {
                command
                    .as_std()
                    .get_envs()
                    .find(|(name, _)| *name == OsStr::new("PATH"))
                    .and_then(|(_, value)| value)
                    .map(OsStr::to_owned)
            };
            assert_eq!(command.as_std().get_program(), program.as_os_str());
            assert_eq!(path(&command), Some(selected.clone()));
            assert_eq!(path(&newer), Some(refreshed.clone()));
            let variable = crate::account::home_variable(provider).unwrap();
            assert_eq!(
                command
                    .as_std()
                    .get_envs()
                    .find(|(name, _)| *name == OsStr::new(variable))
                    .and_then(|(_, value)| value),
                Some(home.as_os_str()),
                "the captured System home must stay explicit too"
            );
        }
    }
}

#[test]
fn status_path_preserves_absence_empty_and_non_unicode_snapshots() {
    use std::{ffi::OsStr, os::unix::ffi::OsStringExt, path::Path};

    let account = crate::account::Account::managed(
        crate::agent::Provider::Codex,
        uuid::Uuid::from_u128(8),
        "/cagents-fixture/reserved-home".into(),
    );
    for selected in [
        None,
        Some(std::ffi::OsString::new()),
        Some(std::ffi::OsString::from_vec(
            b"/cagents-fixture/runtime-\xff/bin".to_vec(),
        )),
    ] {
        let command = super::status_command_with_path(
            &account,
            Path::new("/cagents-fixture/selected-cli/bin/codex"),
            selected.as_deref(),
        )
        .unwrap();
        let actual = command
            .as_std()
            .get_envs()
            .find(|(name, _)| *name == OsStr::new("PATH"))
            .map(|(_, value)| value);
        assert_eq!(actual, selected.as_deref().map(Some));
    }
}

struct FakeRun {
    outcome: Result<LoginStatus, String>,
    record: String,
    selected_home: std::path::PathBuf,
}

async fn fake_status(provider: crate::agent::Provider, body: &str) -> FakeRun {
    use crate::{
        account::{home::Homes, recovery::Candidate, registry::Store},
        host::{AccountRecovery, AccountReservation},
    };
    use std::{fs, os::unix::fs::PermissionsExt};

    // This fake never resolves a provider from PATH or changes the caller's
    // HOME, authority or credentials. Every file belongs to this temp root.
    let directory = tempfile::tempdir().unwrap();
    assert!(directory.path().is_absolute());
    let homes = Homes::at(directory.path().join("accounts"));
    let key = uuid::Uuid::new_v4();
    let selected_home = homes.path(provider, key);
    fs::create_dir_all(&selected_home).unwrap();
    let store = Store::at(directory.path().join("registry"));
    let host = AccountReservation::test_host();
    let recovery =
        AccountRecovery::test_install(&host, store, &homes, Candidate { provider, key }).unwrap();
    let account = recovery.account();
    let program = directory.path().join("fake-provider");
    let record = directory.path().join("status-record");
    let quoted_record = format!("'{}'", record.to_str().unwrap().replace('\'', "'\\''"));
    let variable = crate::account::home_variable(provider).unwrap();
    let script = format!(
        "#!/bin/sh\nset -eu\nrecord={quoted_record}\nprintf 'arg:%s\\n' \"$@\" > \"$record\"\nprintf 'home:%s\\n' \"${{{variable}-unset}}\" >> \"$record\"\n{body}\n"
    );
    fs::write(&program, script).unwrap();
    fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
    let permit = recovery
        .status_permit(account.clone(), program.clone())
        .unwrap();
    let search_path = Some(directory.path().join("private-cli-bin").into_os_string());
    let outcome = super::run(account, program, search_path, permit).await;
    let record = fs::read_to_string(record).unwrap_or_default();
    // Keep the actual host and master recovery alive until the complete
    // status path, including owned process-group cleanup, has returned.
    drop(recovery);
    drop(host);
    FakeRun {
        outcome,
        record,
        selected_home,
    }
}

#[tokio::test]
async fn claude_recovery_asks_only_for_status_in_the_pinned_home() {
    let run = fake_status(
        crate::agent::Provider::Claude,
        r#"[ "$#" = 3 ] || exit 17
[ "$1" = auth ] && [ "$2" = status ] && [ "$3" = --json ] || exit 18
printf '%s\n' '{"loggedIn":true,"email":"claude-fake@example.invalid"}'"#,
    )
    .await;
    assert_eq!(
        signed_in(run.outcome).email.as_deref(),
        Some("claude-fake@example.invalid")
    );
    assert_eq!(
        run.record,
        format!(
            "arg:auth\narg:status\narg:--json\nhome:{}\n",
            run.selected_home.display()
        )
    );
}

#[tokio::test]
async fn claude_recovery_refuses_positive_status_from_a_failed_process() {
    let run = fake_status(
        crate::agent::Provider::Claude,
        r#"printf '%s\n' '{"loggedIn":true,"email":"failed-fake@example.invalid"}'
exit 23"#,
    )
    .await;
    assert!(run.outcome.is_err());
    assert!(run.record.starts_with("arg:auth\narg:status\narg:--json\n"));
}

#[tokio::test]
async fn claude_recovery_accepts_explicit_signed_out_with_exit_one() {
    let run = fake_status(
        crate::agent::Provider::Claude,
        r#"printf '%s\n' '{"loggedIn":false}'
exit 1"#,
    )
    .await;
    assert_signed_out(run.outcome);
    assert!(run.record.starts_with("arg:auth\narg:status\narg:--json\n"));
}

const CODEX_STATUS: &str = r#"[ "$#" = 1 ] && [ "$1" = app-server ] || exit 17
while IFS= read -r line; do
    printf 'request:%s\n' "$line" >> "$record"
    case "$line" in
        *'"method":"initialize"'*) printf '%s\n' '{"id":1,"result":{}}' ;;
        *'"method":"initialized"'*) ;;
        *'"method":"account/read"'*) printf '%s\n' '{"id":2,"result":{"account":{"type":"chatgpt","email":"codex-fake@example.invalid"},"requiresOpenaiAuth":true}}' ;;
        *) exit 18 ;;
    esac
done"#;

#[tokio::test]
async fn codex_recovery_requests_account_identity_without_quota_or_models() {
    let run = fake_status(crate::agent::Provider::Codex, CODEX_STATUS).await;
    assert_eq!(
        signed_in(run.outcome).email.as_deref(),
        Some("codex-fake@example.invalid")
    );
    let lines = run.record.lines().collect::<Vec<_>>();
    assert_eq!(lines[0], "arg:app-server");
    assert_eq!(lines[1], format!("home:{}", run.selected_home.display()));
    let frames = lines[2..]
        .iter()
        .map(|line| serde_json::from_str::<Value>(line.strip_prefix("request:").unwrap()).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(frames.len(), 3);
    assert_eq!(frames[0]["method"], "initialize");
    assert_eq!(frames[0]["id"], 1);
    assert_eq!(frames[1], json!({"method": "initialized"}));
    assert_eq!(
        frames[2],
        json!({"id": 2, "method": "account/read", "params": {}})
    );
}

#[tokio::test]
async fn codex_recovery_refuses_positive_status_from_a_failed_process() {
    let body = format!("{CODEX_STATUS}\nexit 23");
    let run = fake_status(crate::agent::Provider::Codex, &body).await;
    assert!(run.outcome.is_err());
    assert!(run.record.contains("\"method\":\"account/read\""));
}
