//! Bounded transport for Linear's official GraphQL endpoint.
use std::{
    io::{self, Read, Write},
    process::{Command, Stdio},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde_json::{Value, json};

use super::Error;

const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
const MAX_STDERR_BYTES: usize = 16 * 1024;

pub struct Client {
    api_key: String,
    workspace: Option<String>,
}

impl Client {
    /// Named workspaces use CAGENTS_LINEAR_API_KEY_<UPPERCASE_NAME>.
    /// Names contain only ASCII letters, digits, and underscores.
    pub fn from_workspace(workspace: Option<&str>) -> Result<Self, Error> {
        let variable = workspace_variable(workspace)?;
        let api_key = resolve_key(
            &variable,
            || std::env::var(&variable).ok(),
            || super::credentials::get(workspace),
        )?;
        let mut client = Self::from_key(api_key)?;
        client.workspace = workspace.map(str::to_ascii_uppercase);
        Ok(client)
    }

    pub fn workspace(&self) -> Option<&str> {
        self.workspace.as_deref()
    }

    pub fn validate_key(api_key: &str) -> Result<(), Error> {
        if api_key.trim().is_empty() || api_key.bytes().any(|byte| byte.is_ascii_control()) {
            return Err(Error::new(
                "linear_auth_invalid",
                "Linear personal API key is empty or contains control characters",
            ));
        }
        Ok(())
    }

    pub fn from_key(api_key: String) -> Result<Self, Error> {
        Self::validate_key(&api_key)?;
        Ok(Self {
            api_key,
            workspace: None,
        })
    }

    pub fn request(&self, query: &str, variables: Value, mutation: bool) -> Result<Value, Error> {
        self.request_with(query, variables, mutation, curl_request)
    }

    fn request_with(
        &self,
        query: &str,
        variables: Value,
        mutation: bool,
        transport: impl FnMut(&str) -> Result<Vec<u8>, Error>,
    ) -> Result<Value, Error> {
        self.request_with_wait(query, variables, mutation, transport, thread::sleep)
    }

    fn request_with_wait(
        &self,
        query: &str,
        variables: Value,
        mutation: bool,
        mut transport: impl FnMut(&str) -> Result<Vec<u8>, Error>,
        mut wait: impl FnMut(Duration),
    ) -> Result<Value, Error> {
        let body = json!({ "query": query, "variables": variables }).to_string();
        let config = format!(
            "url = \"https://api.linear.app/graphql\"\n\
             request = \"POST\"\n\
             header = \"Content-Type: application/json\"\n\
             header = \"Authorization: {}\"\n\
             header = \"public-file-urls-expire-in: 300\"\n\
             data-binary = \"{}\"\n",
            config_escape(&self.api_key),
            config_escape(&body)
        );
        for attempt in 0..3 {
            let result = transport(&config)
                .map_err(|error| {
                    if mutation {
                        unconfirmed("The transport did not confirm the write")
                    } else {
                        error
                    }
                })
                .and_then(|output| parse_response(&output, mutation));
            match result {
                Err(error) if !mutation && attempt < 2 && error.kind == "linear_rate_limited" => {
                    let delay = error
                        .details
                        .as_ref()
                        .and_then(|details| details["retry_after_ms"].as_u64())
                        .unwrap_or(250 * (attempt + 1));
                    // Long windows belong to the caller. Never park an agent
                    // or UI worker for minutes, or retry earlier than the API says.
                    if delay > 2_000 {
                        return Err(error);
                    }
                    wait(Duration::from_millis(delay));
                }
                result => {
                    return result.map_err(|mut error| {
                        if let Some(details) = &mut error.details {
                            redact_secret(details, &self.api_key);
                        }
                        error
                    });
                }
            }
        }
        unreachable!("bounded retry loop returns its last result")
    }
}

pub(crate) fn workspace_variable(workspace: Option<&str>) -> Result<String, Error> {
    match workspace {
        None => Ok("LINEAR_API_KEY".to_owned()),
        Some(name)
            if !name.is_empty()
                && name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_') =>
        {
            Ok(format!(
                "CAGENTS_LINEAR_API_KEY_{}",
                name.to_ascii_uppercase()
            ))
        }
        Some(_) => Err(Error::new(
            "linear_workspace_invalid",
            "Workspace names must contain only ASCII letters, digits, and underscores",
        )),
    }
}

fn resolve_key(
    variable: &str,
    environment: impl FnOnce() -> Option<String>,
    keyring: impl FnOnce() -> Result<Option<String>, Error>,
) -> Result<String, Error> {
    if let Some(key) = environment() {
        return Ok(key);
    }
    keyring()?.ok_or_else(|| Error::new(
        "linear_auth_required",
        format!("Add a Linear key in Accounts or set {variable}; unlock the desktop keyring if a key is already stored"),
    ))
}

fn config_escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t")
}

fn redact_secret(value: &mut Value, secret: &str) {
    match value {
        Value::String(text) => *text = text.replace(secret, "[REDACTED]"),
        Value::Array(values) => {
            for value in values {
                redact_secret(value, secret);
            }
        }
        Value::Object(values) => {
            let previous = std::mem::take(values);
            for (key, mut value) in previous {
                redact_secret(&mut value, secret);
                values.insert(key.replace(secret, "[REDACTED]"), value);
            }
        }
        _ => {}
    }
}

fn curl_request(config: &str) -> Result<Vec<u8>, Error> {
    // --disable must be first: user curl configuration could otherwise enable
    // traces, redirects, proxies, or additional outputs containing credentials.
    let mut child = Command::new("curl")
        .args([
            "--disable",
            "--silent",
            "--proto",
            "=https",
            "--proto-redir",
            "=https",
            "--max-redirs",
            "0",
            "--connect-timeout",
            "10",
            "--max-time",
            "45",
            "--max-filesize",
            "8388608",
            "--include",
            "--write-out",
            "\n%{http_code}",
            "--config",
            "-",
        ])
        .env_remove("SSLKEYLOGFILE")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| Error::new("linear_transport", format!("Cannot start curl: {error}")))?;
    // Drain both pipes concurrently, even after reaching the memory bound.
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let output_reader =
        thread::spawn(move || read_bounded(stdout, MAX_RESPONSE_BYTES + 32 * 1024 + 4));
    let error_reader = thread::spawn(move || read_bounded(stderr, MAX_STDERR_BYTES));
    let write_result = child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(config.as_bytes());
    if write_result.is_err() {
        let _ = child.kill();
    }
    let status = child.wait();
    let output = output_reader.join();
    let diagnostic = error_reader.join();
    // Never surface curl stderr: even error diagnostics can include secrets.
    if write_result.is_err() || diagnostic.is_err() {
        return Err(Error::new("linear_transport", "Linear transport failed"));
    }
    let status = status.map_err(|_| Error::new("linear_transport", "Cannot wait for curl"))?;
    if !status.success() {
        return Err(Error::new(
            "linear_transport",
            format!("Linear transport failed (curl exit {:?})", status.code()),
        ));
    }
    let (output, exceeded) = output
        .map_err(|_| Error::new("linear_transport", "Linear response reader failed"))?
        .map_err(|_| Error::new("linear_transport", "Cannot read Linear response"))?;
    if exceeded {
        return Err(Error::new(
            "linear_response_too_large",
            "Linear response exceeds 8 MiB; reduce page size or requested fields",
        ));
    }
    Ok(output)
}

fn read_bounded(mut reader: impl Read, limit: usize) -> io::Result<(Vec<u8>, bool)> {
    let mut output = Vec::new();
    let mut exceeded = false;
    let mut buffer = [0; 8192];
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            return Ok((output, exceeded));
        }
        let keep = count.min(limit.saturating_sub(output.len()));
        output.extend_from_slice(&buffer[..keep]);
        exceeded |= keep < count;
    }
}

fn unconfirmed(reason: &str) -> Error {
    Error::new(
        "linear_write_unconfirmed",
        format!(
            "{reason}. The write may have succeeded. Read the issue, comments, or attachments to verify before retrying; writes are never retried automatically"
        ),
    )
}

fn parse_response(output: &[u8], mutation: bool) -> Result<Value, Error> {
    let malformed = || {
        if mutation {
            unconfirmed("Linear returned an invalid response")
        } else {
            Error::new(
                "linear_response_invalid",
                "Linear returned an invalid response",
            )
        }
    };
    let separator = output
        .iter()
        .rposition(|byte| *byte == b'\n')
        .ok_or_else(malformed)?;
    let status: u16 = std::str::from_utf8(&output[separator + 1..])
        .ok()
        .and_then(|status| status.parse().ok())
        .ok_or_else(malformed)?;
    let (body, headers) = response_body(&output[..separator]);
    let response: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let errors = response.get("errors").and_then(Value::as_array);
    let has_errors = errors.is_some_and(|errors| !errors.is_empty());
    let has_data = response.get("data").is_some_and(|data| !data.is_null());
    if mutation && (status >= 500 || (has_errors && has_data)) {
        return Err(unconfirmed("Linear did not fully confirm the write"));
    }
    let rate_limited = status == 429
        || errors.is_some_and(|errors| {
            errors
                .iter()
                .any(|error| error["extensions"]["code"] == "RATELIMITED")
        });
    if rate_limited {
        return Err(Error::new(
            "linear_rate_limited",
            "Linear rate limit reached; wait before making another request",
        )
        .details(rate_limit_details(&headers, epoch_ms())));
    }
    if status == 401 || status == 403 {
        return Err(Error::new(
            "linear_auth_rejected",
            "Linear rejected the API key or its permissions",
        ));
    }
    if has_errors {
        // Execution errors can null out the whole GraphQL data object after a
        // write occurred. Only explicit pre-execution rejection codes establish
        // that retrying is safe; an unknown code must require readback.
        let rejected_before_execution = errors.is_some_and(|errors| {
            errors.iter().all(|error| {
                matches!(
                    error["extensions"]["code"].as_str(),
                    Some("GRAPHQL_VALIDATION_FAILED" | "GRAPHQL_PARSE_FAILED" | "UNAUTHENTICATED")
                )
            })
        });
        if mutation && !rejected_before_execution {
            return Err(unconfirmed("Linear returned a write execution error")
                .details(json!({"errors": errors})));
        }
        return Err(Error::new(
            if has_data {
                "linear_partial_response"
            } else {
                "linear_graphql"
            },
            "Linear returned GraphQL errors; the requested operation was not fully successful",
        )
        .details(json!({"errors": errors, "partialData": has_data})));
    }
    if !(200..300).contains(&status) {
        return Err(Error::new(
            "linear_http",
            format!("Linear returned HTTP {status}"),
        ));
    }
    response
        .get("data")
        .filter(|data| !data.is_null())
        .cloned()
        .ok_or_else(malformed)
}

/// Curl may include proxy CONNECT or interim response headers. The final
/// response's headers determine the reset; nothing from these headers is logged.
fn response_body(mut body: &[u8]) -> (&[u8], Vec<(String, String)>) {
    let mut headers = Vec::new();
    while body.starts_with(b"HTTP/") {
        let Some(end) = body.windows(4).position(|part| part == b"\r\n\r\n") else {
            break;
        };
        headers = String::from_utf8_lossy(&body[..end])
            .lines()
            .skip(1)
            .filter_map(|line| line.split_once(':'))
            .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
            .collect();
        body = &body[end + 4..];
    }
    (body, headers)
}

fn epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn rate_limit_details(headers: &[(String, String)], now: u64) -> Value {
    let header = |name: &str| {
        headers
            .iter()
            .rev()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    };
    let reset = [
        "x-ratelimit-requests-reset",
        "x-ratelimit-endpoint-requests-reset",
        "x-ratelimit-complexity-reset",
    ]
    .into_iter()
    .filter_map(|name| header(name)?.parse::<u64>().ok())
    .max();
    let retry = header("retry-after").and_then(|value| {
        value
            .parse::<u64>()
            .ok()
            .map(|seconds| seconds.saturating_mul(1_000))
            .or_else(|| http_date_ms(value).map(|date| date.saturating_sub(now)))
    });
    let delay = reset
        .map(|reset| reset.saturating_sub(now))
        .into_iter()
        .chain(retry)
        .max();
    json!({"retryable": true, "retry_after_ms": delay, "reset_at": delay.map(|delay| now.saturating_add(delay))})
}

fn http_date_ms(value: &str) -> Option<u64> {
    // IMF-fixdate is always English/GMT and independent of desktop locale.
    let fields: Vec<_> = value.split_whitespace().collect();
    if fields.len() != 6 || fields[5] != "GMT" {
        return None;
    }
    let month = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ]
    .iter()
    .position(|month| *month == fields[2])? as i64
        + 1;
    let year = fields[3].parse::<i64>().ok()?;
    let day = fields[1].parse::<i64>().ok()?;
    let time = fields[4]
        .split(':')
        .map(str::parse::<i64>)
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    if !(1970..=9999).contains(&year)
        || !(1..=31).contains(&day)
        || time.len() != 3
        || time[0] > 23
        || time[1] > 59
        || time[2] > 59
        || time.iter().any(|n| *n < 0)
    {
        return None;
    }
    let year = year - i64::from(month <= 2);
    let era = year / 400;
    let yoe = year - era * 400;
    let shifted_month = month + if month > 2 { -3 } else { 9 };
    let days = era * 146097 + yoe * 365 + yoe / 4 - yoe / 100 + (153 * shifted_month + 2) / 5 + day
        - 1
        - 719468;
    let seconds = days
        .checked_mul(86400)?
        .checked_add(time[0] * 3600 + time[1] * 60 + time[2])?;
    u64::try_from(seconds).ok()?.checked_mul(1000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn environment_overrides_keyring_and_keyring_works_without_environment() {
        assert_eq!(
            resolve_key("LINEAR_API_KEY", || None, || Ok(Some("vault-key".into()))).unwrap(),
            "vault-key"
        );
        assert_eq!(
            resolve_key(
                "LINEAR_API_KEY",
                || Some("env-key".into()),
                || panic!("keyring must not be touched when overridden")
            )
            .unwrap(),
            "env-key"
        );
        assert_eq!(
            resolve_key("LINEAR_API_KEY", || None, || Ok(None))
                .unwrap_err()
                .kind,
            "linear_auth_required"
        );
    }

    #[test]
    fn reset_headers_are_epoch_milliseconds_and_retry_after_is_seconds() {
        let details = rate_limit_details(
            &[
                ("retry-after".into(), "2".into()),
                ("x-ratelimit-requests-reset".into(), "2500".into()),
                ("x-ratelimit-endpoint-requests-reset".into(), "3000".into()),
            ],
            1000,
        );
        assert_eq!(
            details,
            json!({"retryable":true,"retry_after_ms":2000,"reset_at":3000})
        );
        let conflicting = rate_limit_details(
            &[
                ("retry-after".into(), "10".into()),
                ("x-ratelimit-requests-reset".into(), "2500".into()),
            ],
            1000,
        );
        assert_eq!(conflicting["retry_after_ms"], 10_000);
        assert_eq!(conflicting["reset_at"], 11_000);
        assert_eq!(
            http_date_ms("Sun, 06 Nov 1994 08:49:37 GMT"),
            Some(784111777000)
        );
        let response = b"HTTP/1.1 429 Too Many Requests\r\nRetry-After: 2\r\nX-RateLimit-Requests-Reset: 3000\r\n\r\n{}\n429";
        let error = parse_response(response, false).unwrap_err();
        assert_eq!(error.kind, "linear_rate_limited");
        assert_eq!(error.details.as_ref().unwrap()["retry_after_ms"], 2000);
        assert_eq!(error.details.unwrap()["retryable"], true);
    }

    #[test]
    fn rate_limited_reads_retry_at_most_twice_and_mutations_never_retry() {
        let client = Client::from_key("key".into()).unwrap();
        for mutation in [false, true] {
            let mut calls = 0;
            let mut sleeps = Vec::new();
            let error = client
                .request_with_wait(
                    "query { viewer { id } }",
                    json!({}),
                    mutation,
                    |_| {
                        calls += 1;
                        Ok(
                            b"HTTP/1.1 429 Too Many Requests\r\nRetry-After: 0\r\n\r\n{}\n429"
                                .to_vec(),
                        )
                    },
                    |delay| sleeps.push(delay),
                )
                .unwrap_err();
            assert_eq!(error.kind, "linear_rate_limited");
            assert_eq!(calls, if mutation { 1 } else { 3 });
            assert_eq!(sleeps.len(), if mutation { 0 } else { 2 });
        }
        let mut calls = 0;
        client
            .request_with_wait(
                "query { viewer { id } }",
                json!({}),
                false,
                |_| {
                    calls += 1;
                    Ok(if calls == 1 {
                        b"{}\n429".to_vec()
                    } else {
                        b"{\"data\":{\"viewer\":{\"id\":\"ok\"}}}\n200".to_vec()
                    })
                },
                |_| {},
            )
            .unwrap();
        assert_eq!(calls, 2);
    }

    #[test]
    fn long_reset_windows_are_returned_to_the_caller_without_early_retry() {
        let client = Client::from_key("key".into()).unwrap();
        let mut calls = 0;
        let error = client
            .request_with_wait(
                "query { viewer { id } }",
                json!({}),
                false,
                |_| {
                    calls += 1;
                    Ok(
                        b"HTTP/1.1 429 Too Many Requests\r\nRetry-After: 3600\r\n\r\n{}\n429"
                            .to_vec(),
                    )
                },
                |_| panic!("must not sleep past the bounded retry window"),
            )
            .unwrap_err();
        assert_eq!(calls, 1);
        assert_eq!(error.details.unwrap()["retry_after_ms"], 3_600_000);
    }

    #[test]
    fn named_workspace_is_explicit_and_safe() {
        assert_eq!(workspace_variable(None).unwrap(), "LINEAR_API_KEY");
        assert_eq!(
            workspace_variable(Some("team_2")).unwrap(),
            "CAGENTS_LINEAR_API_KEY_TEAM_2"
        );
        for name in ["", "team-two", "../key", "héllo"] {
            assert!(workspace_variable(Some(name)).is_err());
        }
    }

    #[test]
    fn credentials_and_payload_are_escaped_in_stdin_configuration() {
        let client = Client::from_key("key\\\"value".into()).unwrap();
        let result = client
            .request_with(
                "query { viewer { id } }",
                json!({"text":"a\nb"}),
                false,
                |config| {
                    assert!(config.contains("Authorization: key\\\\\\\"value"));
                    assert!(config.contains("public-file-urls-expire-in: 300"));
                    assert!(config.contains("https://api.linear.app/graphql"));
                    Ok(b"{\"data\":{\"viewer\":{\"id\":\"123\"}}}\n200".to_vec())
                },
            )
            .unwrap();
        assert_eq!(result["viewer"]["id"], "123");
        assert!(Client::from_key("key\r\nheader".into()).is_err());
    }

    #[test]
    fn rate_limits_include_graphql_http_400() {
        for response in [
            "{\"errors\":[{\"extensions\":{\"code\":\"RATELIMITED\"}}]}\n400",
            "rate limit\n429",
        ] {
            assert_eq!(
                parse_response(response.as_bytes(), true).unwrap_err().kind,
                "linear_rate_limited"
            );
        }
    }

    #[test]
    fn partial_reads_fail_and_partial_writes_require_readback() {
        let response =
            b"{\"data\":{\"issueUpdate\":null},\"errors\":[{\"message\":\"failure\"}]}\n200";
        assert_eq!(
            parse_response(response, false).unwrap_err().kind,
            "linear_partial_response"
        );
        assert_eq!(
            parse_response(response, true).unwrap_err().kind,
            "linear_write_unconfirmed"
        );
        for response in [&b"bad response\n200"[..], &b"server failure\n502"[..]] {
            assert_eq!(
                parse_response(response, true).unwrap_err().kind,
                "linear_write_unconfirmed"
            );
        }
        assert_eq!(
            parse_response(
                b"{\"data\":null,\"errors\":[{\"message\":\"execution error\"}]}\n200",
                true
            )
            .unwrap_err()
            .kind,
            "linear_write_unconfirmed"
        );
        assert_eq!(
            parse_response(
                b"{\"errors\":[{\"extensions\":{\"code\":\"GRAPHQL_VALIDATION_FAILED\"}}]}\n400",
                true
            )
            .unwrap_err()
            .kind,
            "linear_graphql"
        );
    }

    #[test]
    fn mutation_transport_is_called_once_and_never_retried() {
        let client = Client::from_key("key".into()).unwrap();
        let mut calls = 0;
        let error = client
            .request_with("mutation { test }", json!({}), true, |_| {
                calls += 1;
                Err(Error::new("linear_transport", "failure"))
            })
            .unwrap_err();
        assert_eq!(calls, 1);
        assert_eq!(error.kind, "linear_write_unconfirmed");
        assert!(error.message.contains("before retrying"));
    }

    #[test]
    fn reflected_credentials_are_redacted_from_api_errors() {
        let client = Client::from_key("secret-api-key".into()).unwrap();
        let error = client
            .request_with("query { viewer { id } }", json!({}), false, |_| {
                Ok(b"{\"errors\":[{\"message\":\"secret-api-key rejected\",\"extensions\":{\"secret-api-key\":\"secret-api-key\"}}]}\n400".to_vec())
            })
            .unwrap_err();
        let details = error.details.unwrap().to_string();
        assert!(!details.contains("secret-api-key"));
        assert!(details.contains("[REDACTED]"));
    }

    #[test]
    fn bounded_reader_drains_the_entire_stream() {
        let mut input = io::Cursor::new(vec![b'x'; 100]);
        let (output, exceeded) = read_bounded(&mut input, 10).unwrap();
        assert_eq!(output.len(), 10);
        assert!(exceeded);
        assert_eq!(input.position(), 100);
    }
}
