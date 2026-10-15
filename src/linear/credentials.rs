//! Linear credentials are scoped Secret Service items. The helper receives
//! secret values only over stdin; settings, recipes and command arguments never
//! contain them. Keep the CLI lane independent of the desktop dependency graph.
use std::{
    io::{Read, Write},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use super::Error;

const APPLICATION: &str = "com.carson.cagents.linear";
// The complete stdin write fits Linux PIPE_BUF (4096). A helper waiting
// for a locked vault cannot block us before the timeout starts.
const MAX_KEY_BYTES: usize = 4095;

/// A UI input/message whose debug representation cannot disclose its value.
#[derive(Clone, Default)]
pub struct Secret(pub String);

impl std::fmt::Debug for Secret {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("[REDACTED]")
    }
}

fn keyring_error() -> Error {
    Error::new(
        "linear_keyring_unavailable",
        "Cannot access the Linear key in Secret Service. Install secret-tool (libsecret-tools), unlock your desktop keyring, and retry",
    )
}

fn account(workspace: Option<&str>) -> Result<String, Error> {
    super::api::workspace_variable(workspace)?;
    Ok(workspace.map_or_else(|| "default".into(), |name| format!("workspace:{}", name.to_ascii_uppercase())))
}

fn helper(operation: &[&str], workspace: Option<&str>, secret: Option<&str>) -> Result<Option<String>, Error> {
    let account = account(workspace)?;
    let mut child = Command::new("secret-tool")
        .args(operation)
        .args(["application", APPLICATION, "account", &account])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| keyring_error())?;
    let stdout = child.stdout.take().expect("piped output");
    let reader = thread::spawn(move || {
        let mut output = Vec::new();
        stdout.take((MAX_KEY_BYTES + 2) as u64).read_to_end(&mut output).map(|_| output)
    });
    let mut input = child.stdin.take().expect("piped input");
    let written = match secret {
        Some(secret) => input.write_all(secret.as_bytes()),
        None => Ok(()),
    };
    drop(input);
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if written.is_ok() && started.elapsed() < Duration::from_secs(15) => {
                thread::sleep(Duration::from_millis(20));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let output = reader.join().map_err(|_| keyring_error())?.map_err(|_| keyring_error())?;
    let status = status.ok_or_else(keyring_error)?;
    // A missing entry is the helper's documented exit 1 and has no output.
    if operation == ["lookup"] && status.code() == Some(1) && output.is_empty() {
        return Ok(None);
    }
    if !status.success() || written.is_err() || output.len() > MAX_KEY_BYTES {
        return Err(keyring_error());
    }
    if operation == ["lookup"] {
        // Piped secret-tool store/lookup use exact bytes through EOF and do
        // not add a newline. Never alter the credential during a round trip.
        return String::from_utf8(output).map(Some).map_err(|_| keyring_error());
    }
    Ok(None)
}

pub fn get(workspace: Option<&str>) -> Result<Option<String>, Error> {
    helper(&["lookup"], workspace, None)
}

pub fn store(workspace: Option<&str>, secret: &Secret) -> Result<(), Error> {
    super::api::Client::validate_key(&secret.0)?;
    if secret.0.len() > MAX_KEY_BYTES {
        return Err(Error::new("linear_auth_invalid", "Linear key exceeds the size limit"));
    }
    helper(&["store", "--label=Cagents Linear API key"], workspace, Some(&secret.0)).map(|_| ())
}

pub fn remove(workspace: Option<&str>) -> Result<(), Error> {
    helper(&["clear"], workspace, None).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_are_scoped_without_exposing_secrets_in_debug() {
        assert_eq!(account(None).unwrap(), "default");
        assert_eq!(account(Some("team_2")).unwrap(), "workspace:TEAM_2");
        assert!(account(Some("../invalid")).is_err());
        assert!(!format!("{:?}", Secret("test-linear-key".into())).contains("test-linear-key"));
    }
}
