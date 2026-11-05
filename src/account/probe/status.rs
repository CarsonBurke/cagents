//! Strict, status-only login evidence for a reserved account recovery.
//!
//! It never requests quota, models, a provider turn, login or logout.

use std::{
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
    process::Stdio,
};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};

use super::{OUTPUT_LIMIT, Session, TIMEOUT, codex_response, command_with_path};
use crate::{
    account::{Account, Identity},
    agent::Provider,
    host::RecoveryStatusPermit,
};

mod process;
use process::Process;

type StatusSession = Session<Process>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LoginStatus {
    SignedIn(Identity),
    /// The provider explicitly reports no supported managed-account login.
    /// This also covers Codex API-key authentication, which is not a stored
    /// ChatGPT login that this recovery flow can identify and deduplicate.
    SignedOut,
}

/// Only an exact sealed subject/program pair may enter the recovery spawn
/// path. The spawn hook independently revalidates the permit and command at
/// the actual process-creation boundary, including executable-busy retries.
/// `search_path` is the owned snapshot used to select `program`; it is never
/// refreshed between the managed and System status requests.
pub(crate) async fn run(
    account: Account,
    program: PathBuf,
    search_path: Option<OsString>,
    permit: RecoveryStatusPermit,
) -> Result<LoginStatus, String> {
    if !program.is_absolute()
        || permit.account() != &account
        || permit.program() != program.as_path()
    {
        return Err("The account status request does not match its recovery permit".into());
    }
    // Retain the master reservation across provider I/O and explicit cleanup,
    // independently of the clone moved into the blocking spawn closure.
    let _operation = permit.clone();
    tokio::time::timeout(TIMEOUT, async move {
        match account.provider {
            Provider::Claude => claude(account, program, search_path, permit).await,
            Provider::Codex => codex(account, program, search_path, permit).await,
            Provider::Omp => Err("This provider manages its own account homes".into()),
        }
    })
    .await
    .map_err(|_| {
        "The provider did not finish verifying this account in time. Your account files were kept."
            .to_owned()
    })?
}

async fn claude(
    account: Account,
    program: PathBuf,
    search_path: Option<OsString>,
    permit: RecoveryStatusPermit,
) -> Result<LoginStatus, String> {
    let mut command = status_command_with_path(&account, &program, search_path.as_deref())?;
    command.args(["auth", "status", "--json"]);
    let mut group = Process::spawn(command, permit).await?;
    // The extra byte distinguishes an oversized document from a valid JSON
    // prefix cut off exactly at the cap. Do not use Group::stdout(), whose
    // ordinary probe contract exposes at most OUTPUT_LIMIT bytes.
    let read = async {
        let stdout = group
            .stdout
            .take()
            .ok_or_else(|| "The account status output is unavailable".to_owned())?;
        let mut output = Vec::new();
        stdout
            .take(OUTPUT_LIMIT + 1)
            .read_to_end(&mut output)
            .await
            .map_err(|_| "The account status output could not be read".to_owned())?;
        Ok::<_, String>(output)
    };
    let output = tokio::time::timeout(TIMEOUT, read)
        .await
        .map_err(|_| "Claude Code did not answer the account status request in time".to_owned())
        .and_then(|result| result);
    // A failed or oversized answer also requests owned process cleanup. If
    // the overall timeout cancels this future, the worker retains custody.
    let exit_code = finish_exit_code(&mut group).await;
    parse_claude_output(&output?, exit_code?)
}

async fn codex(
    account: Account,
    program: PathBuf,
    search_path: Option<OsString>,
    permit: RecoveryStatusPermit,
) -> Result<LoginStatus, String> {
    let mut command = status_command_with_path(&account, &program, search_path.as_deref())?;
    command.arg("app-server").stdin(Stdio::piped());
    let mut group = Process::spawn(command, permit).await?;
    let stdin = group.stdin.take();
    let stdout = group
        .stdout
        .take()
        .ok_or("The account status output is unavailable")?;
    // Reuse the ordinary JSON framing with this separately owned child and
    // one extra byte, so exhausting the output cap cannot confirm a login.
    let mut session = Session {
        group,
        stdin,
        lines: BufReader::new(stdout.take(OUTPUT_LIMIT + 1)).lines(),
    };
    let result = tokio::time::timeout(TIMEOUT, codex_status(&mut session))
        .await
        .map_err(|_| "Codex did not answer the account status request in time".to_owned())
        .and_then(|result| result);
    let within_limit = status_output_within_limit(&mut session);
    let Session {
        mut group,
        stdin,
        lines,
    } = session;
    drop(stdin);
    drop(lines);
    let exit_code = finish_exit_code(&mut group).await;
    within_limit?;
    if exit_code? != Some(0) {
        return Err("Codex did not complete the account status request successfully".into());
    }
    result
}

async fn codex_status(session: &mut StatusSession) -> Result<LoginStatus, String> {
    session
        .send(&json!({"id": 1, "method": "initialize", "params": {
            "clientInfo": {"name": "cagents", "title": "cagents", "version": env!("CARGO_PKG_VERSION")},
        }}))
        .await?;
    let initialized = codex_response(session, 1)
        .await?
        .map_err(|_| "Codex refused the account status initialization".to_owned())?;
    status_output_within_limit(session)?;
    if !initialized.is_object() {
        return Err("Codex returned an invalid account status initialization".into());
    }
    session.send(&json!({"method": "initialized"})).await?;
    session
        .send(&json!({"id": 2, "method": "account/read", "params": {}}))
        .await?;
    let account = codex_response(session, 2)
        .await?
        .map_err(|_| "Codex could not verify the selected account login".to_owned())?;
    status_output_within_limit(session)?;
    parse_codex_status(&account)
}

fn status_output_within_limit(session: &mut StatusSession) -> Result<(), String> {
    // Lines<BufReader<Take<ChildStdout>>> retains the stream-wide byte cap.
    // A reserved Session starts at OUTPUT_LIMIT + 1; consuming that last
    // byte is always an overflow, including a syntactically valid prefix.
    if session.lines.get_ref().get_ref().limit() == 0 {
        return Err("The account status output exceeded its size limit".into());
    }
    Ok(())
}

#[cfg(test)]
fn status_command(account: &Account, program: &Path) -> Result<tokio::process::Command, String> {
    // Process ownership fixtures supply an absolute fake program and never
    // resolve a provider or ask the shell for PATH.
    status_command_with_path(account, program, None)
}

fn status_command_with_path(
    account: &Account,
    program: &Path,
    search_path: Option<&OsStr>,
) -> Result<tokio::process::Command, String> {
    // This is the same snapshot used to select program, not a later shell
    // refresh that could pair an old CLI with a different Node installation.
    let mut command = command_with_path(account, program, search_path);
    let variable = crate::account::home_variable(account.provider)
        .ok_or_else(|| "This provider has no account status home".to_owned())?;
    let home = account
        .resolved_home()
        .ok_or_else(|| "The account status home is unavailable".to_owned())?;
    // SystemAt is an exact home captured by the selected daemon too. The
    // common command builder leaves System selections implicit; recovery
    // binds this explicit operand without changing its credential policy.
    command.env(variable, home);
    Ok(command)
}

async fn finish_exit_code(group: &mut Process) -> Result<Option<i32>, String> {
    group.finish().await.map(|status| status.code())
}

fn parse_claude_output(output: &[u8], exit_code: Option<i32>) -> Result<LoginStatus, String> {
    if output.len() as u64 > OUTPUT_LIMIT {
        return Err("The account status output exceeded its size limit".into());
    }
    let status: Value = serde_json::from_slice(output)
        .map_err(|_| "Claude Code returned invalid account status output".to_owned())?;
    match (parse_claude_status(&status)?, exit_code) {
        (login @ LoginStatus::SignedIn(_), Some(0)) => Ok(login),
        // Claude documents exit 1 for an explicit signed-out status. Retain
        // compatibility with clients that report that same answer with 0.
        (LoginStatus::SignedOut, Some(0 | 1)) => Ok(LoginStatus::SignedOut),
        _ => Err("Claude Code did not complete the account status request successfully".into()),
    }
}

fn stable_identity(identity: Identity) -> Result<LoginStatus, String> {
    if !identity
        .email
        .as_deref()
        .is_some_and(|email| !email.trim().is_empty() && !email.chars().any(char::is_control))
    {
        return Err("The provider did not name a stable account identity".into());
    }
    Ok(LoginStatus::SignedIn(identity))
}

fn parse_claude_status(status: &Value) -> Result<LoginStatus, String> {
    match status.get("loggedIn").and_then(Value::as_bool) {
        Some(false) => Ok(LoginStatus::SignedOut),
        Some(true) => {
            let identity = crate::account::claude::parse_auth_status(status)
                .map_err(|_| "Claude Code did not confirm the account login".to_owned())?;
            stable_identity(identity)
        }
        None => Err("Claude Code returned an invalid account status answer".into()),
    }
}

fn parse_codex_status(status: &Value) -> Result<LoginStatus, String> {
    match status.get("account") {
        Some(Value::Null)
            if status
                .get("requiresOpenaiAuth")
                .and_then(Value::as_bool)
                .is_some() =>
        {
            Ok(LoginStatus::SignedOut)
        }
        Some(Value::Object(account)) => match account.get("type").and_then(Value::as_str) {
            Some("chatgpt") => {
                let identity = crate::account::codex::parse_account(status)
                    .map_err(|_| "Codex did not confirm the account login".to_owned())?;
                stable_identity(identity)
            }
            Some("apiKey") => Ok(LoginStatus::SignedOut),
            _ => Err("Codex returned an unsupported account status answer".into()),
        },
        _ => Err("Codex returned an invalid account status answer".into()),
    }
}

#[cfg(test)]
#[path = "status/tests.rs"]
mod tests;
