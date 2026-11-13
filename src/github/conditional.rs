//! Conditional REST reads. GitHub answers a request whose `If-None-Match`
//! still matches with `304 Not Modified`, which costs none of the account's
//! rate limit, so a poll can ask whether anything changed before it pays for
//! reading what did.
use super::*;

/// What a conditional read learned about a resource.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Revalidation {
    /// Unchanged since the validator the caller sent.
    NotModified,
    /// Changed, or there was nothing to compare with: the validator to send
    /// next time, when GitHub returned one, what the resource now holds, and
    /// GitHub's time when it answered, to compare its dates with.
    Modified {
        etag: Option<String>,
        body: Vec<u8>,
        date: Option<jiff::Timestamp>,
    },
    /// GitHub refuses the resource itself, such as the issues of a repository
    /// that disabled them, so asking again soon would be refused again.
    Refused,
}

/// How long after a push a pull request with no checks may still get some.
const CHECKS_START_WITHIN: Duration = Duration::from_secs(10 * 60);

/// Whether an open pull request's checks may still change without its
/// `updatedAt` moving, which is all a probe watches: some are pending, or
/// none have reported yet for one updated so lately that its CI may not have
/// started. A listing for which this holds is read again every poll.
pub fn checks_settling(pull_requests: &[PullRequest]) -> bool {
    let recent = |pull_request: &PullRequest| {
        pull_request
            .updated_at
            .parse::<jiff::Timestamp>()
            .is_ok_and(|updated| {
                jiff::Timestamp::now().duration_since(updated)
                    < jiff::SignedDuration::try_from(CHECKS_START_WITHIN).unwrap_or_default()
            })
    };
    pull_requests
        .iter()
        .filter(|pull_request| pull_request.state.0 == "OPEN")
        .any(|pull_request| {
            let checks = pull_request.check_summaries();
            checks.iter().any(|check| check.bucket.0 == "pending")
                || (checks.is_empty() && recent(pull_request))
        })
}

impl<E: ProcessExecutor> GitHubService<E> {
    /// Asks whether the REST resource at `path` changed since `etag`.
    ///
    /// Only a rate limit charges the host's budget: a probe answers whether
    /// to read, so its other outcomes say nothing about the reads it guards,
    /// and a free answer must not clear their backoff.
    pub fn revalidate(
        &self,
        repository: &GitHubRepoKey,
        path: &str,
        etag: Option<&str>,
        cancellation: &CancellationToken,
    ) -> Result<Revalidation, GitHubError> {
        self.with_transport_scope(&repository.authority(), "core", || {
            repository.validate()?;
            if let Some(limit) = polling::rate_limit(&repository.authority()) {
                return Err(GitHubError::RateLimited { limit });
            }
            let command = self.revalidate_command(repository, path, etag);
            let output = self.execute(&command, cancellation)?;
            let head = &output.stdout[..output.stdout.len() - graphql_body(&output.stdout).len()];
            let status = status_code(head);
            // gh reports a 304 as a failure, with the response's headers on
            // standard output.
            if etag.is_some() && status == Some(304) {
                return Ok(Revalidation::NotModified);
            }
            match self.checked_output(output, repository) {
                Ok(output) => {
                    let body = graphql_body(&output.stdout);
                    let head = &output.stdout[..output.stdout.len() - body.len()];
                    Ok(Revalidation::Modified {
                        etag: response_header(head, "etag"),
                        body: body.to_vec(),
                        date: response_header(head, "date").and_then(|date| {
                            jiff::fmt::rfc2822::DateTimeParser::new()
                                .parse_timestamp(date)
                                .ok()
                        }),
                    })
                }
                Err(error) if error.rate_limit().is_some() => {
                    polling::failed(&repository.authority(), &error);
                    Err(error)
                }
                Err(_) if matches!(status, Some(403 | 404 | 410)) => Ok(Revalidation::Refused),
                Err(error) => Err(error),
            }
        })
    }

    fn revalidate_command(
        &self,
        repository: &GitHubRepoKey,
        path: &str,
        etag: Option<&str>,
    ) -> CommandSpec {
        let mut command = self.gh_command([
            OsString::from("api"),
            "--include".into(),
            "--hostname".into(),
            repository.authority().into(),
            "--method".into(),
            "GET".into(),
        ]);
        if let Some(etag) = etag {
            command
                .arguments
                .extend(["--header".into(), format!("If-None-Match: {etag}").into()]);
        }
        command.arguments.push(path.into());
        command
    }
}

/// The status code of the last response in `head`, which gh prints as
/// `HTTP/2.0 304 Not Modified`.
fn status_code(head: &[u8]) -> Option<u16> {
    let head = std::str::from_utf8(head).ok()?;
    head.lines()
        .rfind(|line| line.starts_with("HTTP/"))?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

fn response_header(head: &[u8], name: &str) -> Option<String> {
    String::from_utf8_lossy(head).lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.trim()
            .eq_ignore_ascii_case(name)
            .then(|| value.trim().to_owned())
            .filter(|value| !value.is_empty())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Answer(Mutex<Vec<CommandSpec>>, ProcessOutput);

    impl ProcessExecutor for Answer {
        fn execute(
            &self,
            command: &CommandSpec,
            _: &CancellationToken,
        ) -> Result<ProcessOutput, ProcessFailure> {
            self.0.lock().unwrap().push(command.clone());
            Ok(self.1.clone())
        }
    }

    fn answer(success: bool, stdout: &str, stderr: &str) -> Arc<Answer> {
        Arc::new(Answer(
            Mutex::default(),
            ProcessOutput {
                success,
                exit_code: Some(if success { 0 } else { 1 }),
                signal: None,
                stdout: stdout.as_bytes().to_vec(),
                stderr: stderr.as_bytes().to_vec(),
            },
        ))
    }

    fn repository(host: &str) -> GitHubRepoKey {
        GitHubRepoKey::new(host, None, "owner", "repo").unwrap()
    }

    fn pull_request(state: &str, updated: &str, checks: serde_json::Value) -> PullRequest {
        serde_json::from_value(serde_json::json!({
            "number": 1, "title": "t", "state": state, "headRefName": "b",
            "baseRefName": "main", "updatedAt": updated,
            "url": "https://github.com/owner/repo/pull/1", "statusCheckRollup": checks,
        }))
        .unwrap()
    }

    #[test]
    fn checks_settle_once_none_are_pending_or_ci_has_had_time_to_start() {
        let now = jiff::Timestamp::now().to_string();
        let old = "2026-01-01T00:00:00Z";
        let pending = serde_json::json!([{"__typename": "StatusContext", "context": "ci", "state": "PENDING"}]);
        let passed = serde_json::json!([{"__typename": "StatusContext", "context": "ci", "state": "SUCCESS"}]);
        assert!(checks_settling(&[pull_request(
            "OPEN",
            old,
            pending.clone()
        )]));
        assert!(!checks_settling(&[pull_request("OPEN", &now, passed)]));
        assert!(checks_settling(&[pull_request(
            "OPEN",
            &now,
            serde_json::json!([])
        )]));
        assert!(!checks_settling(&[pull_request(
            "OPEN",
            old,
            serde_json::json!([])
        )]));
        assert!(!checks_settling(&[pull_request("MERGED", &now, pending)]));
    }

    #[test]
    fn a_matching_validator_is_not_modified_and_its_headers_are_sent() {
        let repository = repository("revalidate-304.invalid");
        let executor = answer(
            false,
            "HTTP/2.0 304 Not Modified\r\nEtag: \"abc\"\r\nX-Ratelimit-Remaining: 4000\r\n\r\n",
            "gh: HTTP 304\n",
        );
        let service = GitHubService::new("gh", executor.clone());
        let revalidation = service
            .revalidate(
                &repository,
                "repos/owner/repo/issues?per_page=1",
                Some("W/\"abc\""),
                &CancellationToken::new(),
            )
            .unwrap();
        assert_eq!(revalidation, Revalidation::NotModified);
        let commands = executor.0.lock().unwrap();
        let arguments = commands[0]
            .arguments
            .iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            arguments,
            [
                "api",
                "--include",
                "--hostname",
                "revalidate-304.invalid",
                "--method",
                "GET",
                "--header",
                "If-None-Match: W/\"abc\"",
                "repos/owner/repo/issues?per_page=1",
            ]
        );
        assert!(polling::ready("revalidate-304.invalid", false));
    }

    #[test]
    fn a_changed_resource_returns_its_new_validator() {
        let service = GitHubService::new(
            "gh",
            answer(
                true,
                "HTTP/2.0 200 OK\r\nDate: Thu, 12 Nov 2026 23:59:35 GMT\r\nEtag: W/\"next\"\r\nX-Ratelimit-Remaining: 4000\r\n\r\n[{\"number\":1}]",
                "",
            ),
        );
        assert_eq!(
            service
                .revalidate(
                    &repository("revalidate-200.invalid"),
                    "repos/owner/repo/issues",
                    Some("W/\"old\""),
                    &CancellationToken::new(),
                )
                .unwrap(),
            Revalidation::Modified {
                etag: Some("W/\"next\"".into()),
                body: b"[{\"number\":1}]".to_vec(),
                date: Some("2026-11-12T23:59:35Z".parse().unwrap()),
            }
        );
    }

    #[test]
    fn a_refused_resource_is_refused_and_other_failures_are_errors() {
        let host = "revalidate-410.invalid";
        let gone = GitHubService::new(
            "gh",
            answer(
                false,
                "HTTP/2.0 410 Gone\r\n\r\n{\"message\":\"Issues are disabled for this repo\"}",
                "gh: Issues are disabled for this repo (HTTP 410)\n",
            ),
        );
        assert_eq!(
            gone.revalidate(
                &repository(host),
                "repos/owner/repo/issues",
                Some("W/\"abc\""),
                &CancellationToken::new(),
            )
            .unwrap(),
            Revalidation::Refused
        );
        // A probe's failure leaves the reads it guards to find their own.
        assert!(polling::ready(host, false));
        // Nothing to be unchanged from: a proxy's 304 is not an answer.
        let unexpected = GitHubService::new(
            "gh",
            answer(false, "HTTP/2.0 304 Not Modified\r\n\r\n", "gh: HTTP 304\n"),
        );
        assert!(
            unexpected
                .revalidate(
                    &repository("revalidate-unasked.invalid"),
                    "repos/owner/repo/issues",
                    None,
                    &CancellationToken::new(),
                )
                .is_err()
        );
        let unavailable = GitHubService::new(
            "gh",
            answer(false, "HTTP/2.0 502 Bad Gateway\r\n\r\n", "gh: HTTP 502\n"),
        );
        assert!(
            unavailable
                .revalidate(
                    &repository("revalidate-502.invalid"),
                    "repos/owner/repo/issues",
                    Some("W/\"abc\""),
                    &CancellationToken::new(),
                )
                .is_err()
        );
        assert!(polling::ready("revalidate-502.invalid", false));
    }

    #[test]
    fn an_exhausted_core_quota_is_a_rate_limit_and_leaves_graphql_available() {
        let host = "revalidate-exhausted.invalid";
        let service = GitHubService::new(
            "gh",
            answer(
                false,
                &format!(
                    "HTTP/2.0 403 Forbidden\r\nX-Ratelimit-Remaining: 0\r\nX-Ratelimit-Resource: core\r\nX-Ratelimit-Reset: {}\r\n\r\n{{\"message\":\"API rate limit exceeded\"}}",
                    polling::now() + 3600
                ),
                "gh: API rate limit exceeded (HTTP 403)\n",
            ),
        );
        polling::with_scope("revalidate-account", || {
            let error = service
                .revalidate(
                    &repository(host),
                    "repos/owner/repo/issues",
                    Some("W/\"abc\""),
                    &CancellationToken::new(),
                )
                .unwrap_err();
            assert_eq!(
                error.rate_limit().and_then(|limit| limit.resource),
                Some("core".into())
            );
            assert!(polling::with_resource("graphql", || polling::ready(
                host, false
            )));
            assert!(!polling::with_resource("core", || polling::ready(
                host, false
            )));
        });
    }
}
