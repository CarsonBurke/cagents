//! Linear credentials are scoped Secret Service items. The helper receives
//! secret values only over stdin; settings, recipes and command arguments never
//! contain them. Keep the CLI lane independent of the desktop dependency graph.
use std::{
    io::{self, Read, Write},
    os::fd::AsRawFd,
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
    Ok(workspace.map_or_else(
        || "default".into(),
        |name| format!("workspace:{}", name.to_ascii_uppercase()),
    ))
}

fn helper(
    operation: &[&str],
    workspace: Option<&str>,
    secret: Option<&str>,
) -> Result<Option<String>, Error> {
    let account = account(workspace)?;
    let mut child = Command::new("secret-tool")
        .args(operation)
        .args(["application", APPLICATION, "account", &account])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|_| keyring_error())?;
    let mut stdout = child.stdout.take().expect("piped output");
    let mut stderr = child.stderr.take().expect("piped errors");
    let mut input = child.stdin.take().expect("piped input");
    let started = Instant::now();
    let result = (|| -> Result<Option<String>, Error> {
        for descriptor in [stdout.as_raw_fd(), stderr.as_raw_fd()] {
            // SAFETY: both descriptors stay owned and open for this operation.
            let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFL) };
            if flags < 0
                || unsafe { libc::fcntl(descriptor, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
            {
                return Err(keyring_error());
            }
        }
        if let Some(secret) = secret {
            input
                .write_all(secret.as_bytes())
                .map_err(|_| keyring_error())?;
        }
        drop(input);
        let mut output = Vec::new();
        let mut error_bytes = 0u64;
        let mut stdout_done = false;
        let mut stderr_done = false;
        let mut status = None;
        let mut buffer = [0; 4096];
        let status = loop {
            if started.elapsed() >= Duration::from_secs(15) {
                return Err(keyring_error());
            }
            // Read at most one buffer from each pipe per turn. A noisy helper
            // cannot postpone the original deadline by keeping a pipe ready.
            if !stdout_done {
                match stdout.read(&mut buffer) {
                    Ok(0) => stdout_done = true,
                    Ok(count) if output.len() + count <= MAX_KEY_BYTES => {
                        output.extend_from_slice(&buffer[..count]);
                    }
                    Ok(_) => return Err(keyring_error()),
                    Err(error)
                        if matches!(
                            error.kind(),
                            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                        ) => {}
                    Err(_) => return Err(keyring_error()),
                }
            }
            if !stderr_done {
                // libsecret uses exit 1 for both missing entries and errors.
                // Count diagnostics, retaining and logging none of their bytes.
                match stderr.read(&mut buffer) {
                    Ok(0) => stderr_done = true,
                    Ok(count) => error_bytes = error_bytes.saturating_add(count as u64),
                    Err(error)
                        if matches!(
                            error.kind(),
                            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                        ) => {}
                    Err(_) => return Err(keyring_error()),
                }
            }
            if status.is_none() {
                status = child.try_wait().map_err(|_| keyring_error())?;
            }
            if stdout_done && stderr_done {
                if let Some(status) = status {
                    break status;
                }
            }
            thread::sleep(Duration::from_millis(20));
        };
        // A missing entry exits 1 with neither a value nor error diagnostics.
        if operation == ["lookup"]
            && status.code() == Some(1)
            && output.is_empty()
            && error_bytes == 0
        {
            return Ok(None);
        }
        if !status.success() {
            return Err(keyring_error());
        }
        if operation == ["lookup"] {
            // Piped store/lookup preserve exact bytes through EOF.
            return String::from_utf8(output)
                .map(Some)
                .map_err(|_| keyring_error());
        }
        Ok(None)
    })();
    // No pipe reader escapes this function, including error paths. Always
    // consume this original child's status; never signal a numeric descendant.
    if result.is_err() {
        let _ = child.kill();
    }
    if child.wait().is_err() {
        return Err(keyring_error());
    }
    result
}

pub fn get(workspace: Option<&str>) -> Result<Option<String>, Error> {
    helper(&["lookup"], workspace, None)
}

pub fn store(workspace: Option<&str>, secret: &Secret) -> Result<(), Error> {
    super::api::Client::validate_key(&secret.0)?;
    if secret.0.len() > MAX_KEY_BYTES {
        return Err(Error::new(
            "linear_auth_invalid",
            "Linear key exceeds the size limit",
        ));
    }
    helper(
        &["store", "--label=Cagents Linear API key"],
        workspace,
        Some(&secret.0),
    )
    .map(|_| ())
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
