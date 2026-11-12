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
    /// next time, when GitHub returned one.
    Modified { etag: Option<String> },
}

impl<E: ProcessExecutor> GitHubService<E> {
    /// Asks whether the REST resource at `path` changed since `etag`.
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
            let result = self
                .execute(&command, cancellation)
                .map_err(GitHubError::from)
                .and_then(|output| {
                    let head =
                        &output.stdout[..output.stdout.len() - graphql_body(&output.stdout).len()];
                    // gh reports a 304 as a failure, with the response's
                    // headers on standard output.
                    if etag.is_some() && status_code(head) == Some(304) {
                        return Ok(Revalidation::NotModified);
                    }
                    let output = self.checked_output(output, repository)?;
                    let head =
                        &output.stdout[..output.stdout.len() - graphql_body(&output.stdout).len()];
                    Ok(Revalidation::Modified {
                        etag: response_header(head, "etag"),
                    })
                });
            charge_host(repository, result)
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
                "HTTP/2.0 200 OK\r\nEtag: W/\"next\"\r\nX-Ratelimit-Remaining: 4000\r\n\r\n[{\"number\":1}]",
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
                etag: Some("W/\"next\"".into())
            }
        );
    }

    #[test]
    fn a_304_without_a_validator_sent_and_other_failures_are_errors() {
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
        let gone = GitHubService::new(
            "gh",
            answer(
                false,
                "HTTP/2.0 410 Gone\r\n\r\n{\"message\":\"Issues are disabled for this repo\"}",
                "gh: Issues are disabled for this repo (HTTP 410)\n",
            ),
        );
        assert!(
            gone.revalidate(
                &repository("revalidate-410.invalid"),
                "repos/owner/repo/issues",
                Some("W/\"abc\""),
                &CancellationToken::new(),
            )
            .is_err()
        );
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
