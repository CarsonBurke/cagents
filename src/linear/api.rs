//! Bounded transport for Linear's official GraphQL endpoint.
use std::{
    io::{self, Read, Write},
    process::{Command, Stdio},
    thread,
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
        let api_key = std::env::var(&variable).map_err(|_| {
            Error::new(
                "linear_auth_required",
                format!("Set {variable} to a Linear personal API key"),
            )
        })?;
        let mut client = Self::from_key(api_key)?;
        client.workspace = workspace.map(str::to_ascii_uppercase);
        Ok(client)
    }

    pub fn workspace(&self) -> Option<&str> {
        self.workspace.as_deref()
    }

    fn from_key(api_key: String) -> Result<Self, Error> {
        if api_key.trim().is_empty() || api_key.bytes().any(|byte| byte.is_ascii_control()) {
            return Err(Error::new(
                "linear_auth_invalid",
                "Linear personal API key is empty or contains control characters",
            ));
        }
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
        transport: impl FnOnce(&str) -> Result<Vec<u8>, Error>,
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
        let output = transport(&config).map_err(|error| {
            if mutation {
                unconfirmed("The transport did not confirm the write")
            } else {
                error
            }
        })?;
        parse_response(&output, mutation).map_err(|mut error| {
            if let Some(details) = &mut error.details {
                redact_secret(details, &self.api_key);
            }
            error
        })
    }
}

fn workspace_variable(workspace: Option<&str>) -> Result<String, Error> {
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
    let output_reader = thread::spawn(move || read_bounded(stdout, MAX_RESPONSE_BYTES + 4));
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
    let response: Value = serde_json::from_slice(&output[..separator]).unwrap_or(Value::Null);
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
        ));
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

#[cfg(test)]
mod tests {
    use super::*;

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
