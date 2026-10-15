//! Bounded viewed mutations and standalone published review comments.
use super::*;
pub const VIEWED_BATCH_SIZE: usize = 50;
impl<E: ProcessExecutor> GitHubService<E> {
    pub(super) fn post_single_review_comment(
        &self,
        key: &PullRequestKey,
        commit: &str,
        request: &AddReviewThreadRequest,
        cancellation: &CancellationToken,
    ) -> Result<ReviewMutationOutcome<()>, GitHubError> {
        key.validate()?;
        validate_review_oid(commit)?;
        request.validate()?;
        let endpoint = format!(
            "repos/{}/{}/pulls/{}/comments",
            key.repository.owner, key.repository.name, key.number
        );
        let mut command = self.gh_command([
            "api",
            "--include",
            "--hostname",
            &key.repository.authority(),
            "--method",
            "POST",
            &endpoint,
        ]);
        for (name, value) in [
            ("body", request.body.as_str()),
            ("commit_id", commit),
            ("path", request.path.as_str()),
        ] {
            command
                .arguments
                .extend(["--raw-field".into(), format!("{name}={value}").into()]);
        }
        match request.subject {
            ReviewThreadSubject::File => command
                .arguments
                .extend(["--raw-field".into(), "subject_type=file".into()]),
            ReviewThreadSubject::Lines {
                line,
                side,
                start_line,
                start_side,
            } => {
                command.arguments.extend([
                    "--field".into(),
                    format!("line={line}").into(),
                    "--raw-field".into(),
                    format!("side={}", side.as_graphql()).into(),
                ]);
                if let Some(start) = start_line {
                    command.arguments.extend([
                        "--field".into(),
                        format!("start_line={start}").into(),
                        "--raw-field".into(),
                        format!("start_side={}", start_side.unwrap_or(side).as_graphql()).into(),
                    ]);
                }
            }
        }
        let output = match self.execute(&command, cancellation) {
            Ok(output) => output,
            Err(failure) if failure.started => {
                return Ok(ReviewMutationOutcome::OutcomeUnknown { failure });
            }
            Err(failure) => return Ok(ReviewMutationOutcome::NotApplied { failure }),
        };
        let headers = &output.stdout[..output.stdout.len() - graphql_body(&output.stdout).len()];
        let response =
            serde_json::from_slice::<serde_json::Value>(graphql_body(&output.stdout)).ok();
        let message = response
            .as_ref()
            .and_then(|body| body.get("message"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        if let Some(limit) = polling::RateLimit::classify(
            &format!(
                "{}\n{}\n{}",
                String::from_utf8_lossy(headers),
                message,
                String::from_utf8_lossy(&output.stderr)
            ),
            polling::now(),
        ) {
            polling::failed(&key.repository.host, &GitHubError::RateLimited { limit });
        }
        if output.success
            && response
                .as_ref()
                .is_some_and(|body| body.get("id").is_some())
        {
            return Ok(ReviewMutationOutcome::Applied(()));
        }
        let refused = String::from_utf8_lossy(headers).lines().any(|line| {
            line.starts_with("HTTP/")
                && line
                    .split_whitespace()
                    .nth(1)
                    .is_some_and(|status| status.starts_with('4'))
        }) || String::from_utf8_lossy(&output.stderr).contains("HTTP 4");
        let failure = parse_warning(
            &self.gh,
            output,
            "GitHub did not confirm the single comment".into(),
        );
        Ok(if refused {
            ReviewMutationOutcome::NotApplied { failure }
        } else {
            ReviewMutationOutcome::OutcomeUnknown { failure }
        })
    }
    pub(super) fn viewed_batch(
        &self,
        repository: &GitHubRepoKey,
        pr: &str,
        paths: &[String],
        viewed: bool,
        cancellation: &CancellationToken,
    ) -> Vec<(String, Result<ReviewMutationOutcome<()>, GitHubError>)> {
        let validate = || -> Result<(), GitHubError> {
            repository.validate()?;
            validate_node_id("pull_request_id", pr)?;
            if paths.len() > VIEWED_BATCH_SIZE {
                return Err(GitHubError::Inspection(
                    "Viewed batch exceeds its chunk limit".into(),
                ));
            }
            for path in paths {
                validate_review_path(path)?;
            }
            Ok(())
        };
        if let Err(error) = validate() {
            return paths
                .iter()
                .map(|path| {
                    (
                        path.clone(),
                        Err(GitHubError::Inspection(error.to_string())),
                    )
                })
                .collect();
        }
        if paths.is_empty() {
            return Vec::new();
        }
        let operation = if viewed {
            "markFileAsViewed"
        } else {
            "unmarkFileAsViewed"
        };
        // JSON strings also form valid GraphQL string literals; paths cannot add syntax.
        let fields = paths
            .iter()
            .enumerate()
            .map(|(i, path)| {
                format!(
                    "f{i}:{operation}(input:{{pullRequestId:$pr,path:{}}}){{clientMutationId}}",
                    serde_json::to_string(path).expect("string serialization")
                )
            })
            .collect::<String>();
        let query = format!("mutation($pr:ID!){{{fields}}}");
        // Deep repository paths can make even fifty aliases exceed Linux's
        // per-argument ceiling. Keep large documents off argv.
        let command = if query.len() > 64 * 1024 {
            self.gh_command([
                "api",
                "graphql",
                "--include",
                "--hostname",
                &repository.authority(),
                "--method",
                "POST",
                "--input",
                "-",
            ])
            .stdin(
                serde_json::to_vec(&serde_json::json!({"query": query, "variables": {"pr": pr}}))
                    .expect("GraphQL envelope serialization"),
            )
        } else {
            self.graphql_command(repository, &query, vec![("pr", pr.into())])
        };
        let output = match self.execute(&command, cancellation) {
            Ok(output) => output,
            Err(failure) => {
                return paths
                    .iter()
                    .map(|path| {
                        (
                            path.clone(),
                            Ok(if failure.started {
                                ReviewMutationOutcome::OutcomeUnknown {
                                    failure: failure.clone(),
                                }
                            } else {
                                ReviewMutationOutcome::NotApplied {
                                    failure: failure.clone(),
                                }
                            }),
                        )
                    })
                    .collect();
            }
        };
        let body = serde_json::from_slice::<serde_json::Value>(graphql_body(&output.stdout)).ok();
        if let Some(limit) = polling::RateLimit::classify(
            &format!(
                "{}\n{}",
                format!(
                    "{}\n{}",
                    String::from_utf8_lossy(
                        &output.stdout[..output.stdout.len() - graphql_body(&output.stdout).len()]
                    ),
                    body.as_ref()
                        .and_then(|body| body.get("errors"))
                        .map(ToString::to_string)
                        .unwrap_or_default()
                ),
                String::from_utf8_lossy(&output.stderr)
            ),
            polling::now(),
        ) {
            polling::failed(&repository.host, &GitHubError::RateLimited { limit });
        }
        paths
            .iter()
            .enumerate()
            .map(|(index, path)| {
                let alias = format!("f{index}");
                if body
                    .as_ref()
                    .and_then(|body| body.pointer(&format!("/data/{alias}")))
                    .is_some_and(|value| !value.is_null())
                {
                    return (path.clone(), Ok(ReviewMutationOutcome::Applied(())));
                }
                let errors: Vec<_> = body
                    .as_ref()
                    .and_then(|body| body.get("errors"))
                    .and_then(serde_json::Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter(|error| {
                        error
                            .get("path")
                            .and_then(|path| path.get(0))
                            .and_then(serde_json::Value::as_str)
                            .is_none_or(|field| field == alias)
                    })
                    .collect();
                let refused = !errors.is_empty()
                    && (body.as_ref().is_some_and(|body| body.get("data").is_none())
                        || errors.iter().all(|error| {
                            error
                                .get("type")
                                .and_then(serde_json::Value::as_str)
                                .is_some_and(mutation_refusal_type)
                        }));
                let message = errors
                    .iter()
                    .filter_map(|error| error.get("message").and_then(serde_json::Value::as_str))
                    .collect::<Vec<_>>()
                    .join("; ");
                let failure = parse_warning(
                    &self.gh,
                    output.clone(),
                    format!("GitHub did not confirm {path}: {message}"),
                );
                (
                    path.clone(),
                    Ok(if refused {
                        ReviewMutationOutcome::NotApplied { failure }
                    } else {
                        ReviewMutationOutcome::OutcomeUnknown { failure }
                    }),
                )
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Default)]
    struct Recorder {
        commands: Mutex<Vec<CommandSpec>>,
        outputs: Mutex<VecDeque<ProcessOutput>>,
    }
    impl ProcessExecutor for Recorder {
        fn execute(
            &self,
            command: &CommandSpec,
            _: &CancellationToken,
        ) -> Result<ProcessOutput, ProcessFailure> {
            self.commands.lock().unwrap().push(command.clone());
            Ok(self
                .outputs
                .lock()
                .unwrap()
                .pop_front()
                .expect("scripted response"))
        }
    }
    fn output(body: serde_json::Value) -> ProcessOutput {
        ProcessOutput {
            success: true,
            exit_code: Some(0),
            signal: None,
            stdout: serde_json::to_vec(&body).unwrap(),
            stderr: vec![],
        }
    }
    #[test]
    fn batches_fifty_aliases_and_maps_only_failed_alias_to_failed_path() {
        let executor = Arc::new(Recorder::default());
        executor
            .outputs
            .lock()
            .unwrap()
            .push_back(output(serde_json::json!({
                "data":{"f0":{"clientMutationId":null},"f1":null,"f2":{"clientMutationId":null}},
                "errors":[{"type":"FORBIDDEN","path":["f1"],"message":"Not permitted"}]
            })));
        let service = GitHubService::new("gh", executor.clone());
        let repository = GitHubRepoKey::new("batch-test.invalid", None, "owner", "repo").unwrap();
        let results = service.viewed_batch(
            &repository,
            "PR_1",
            &["one.rs".into(), "two.rs".into(), "three.rs".into()],
            true,
            &CancellationToken::new(),
        );
        assert!(matches!(
            &results[0].1,
            Ok(ReviewMutationOutcome::Applied(()))
        ));
        assert!(matches!(
            &results[1].1,
            Ok(ReviewMutationOutcome::NotApplied { .. })
        ));
        assert!(matches!(
            &results[2].1,
            Ok(ReviewMutationOutcome::Applied(()))
        ));
        assert_eq!(executor.commands.lock().unwrap().len(), 1);
        let args = executor.commands.lock().unwrap()[0].arguments.clone();
        assert!(
            args.iter()
                .any(|arg| arg.to_string_lossy().contains("f2:markFileAsViewed"))
        );
        executor.outputs.lock().unwrap().push_back(output(
            serde_json::json!({"data":{"f0":{"clientMutationId":null}}}),
        ));
        assert!(matches!(
            &service.viewed_batch(
                &repository,
                "PR_1",
                &["one.rs".into()],
                false,
                &CancellationToken::new()
            )[0]
            .1,
            Ok(ReviewMutationOutcome::Applied(()))
        ));
        assert!(
            executor.commands.lock().unwrap()[1]
                .arguments
                .iter()
                .any(|arg| arg.to_string_lossy().contains("unmarkFileAsViewed"))
        );
    }
    #[test]
    fn fifty_deep_paths_use_stdin_without_exceeding_the_argument_ceiling() {
        let executor = Arc::new(Recorder::default());
        let data: serde_json::Map<String, serde_json::Value> = (0..50)
            .map(|i| {
                (
                    format!("f{i}"),
                    serde_json::json!({"clientMutationId": null}),
                )
            })
            .collect();
        executor
            .outputs
            .lock()
            .unwrap()
            .push_back(output(serde_json::json!({"data": data})));
        let service = GitHubService::new("gh", executor.clone());
        let repository =
            GitHubRepoKey::new("long-batch-test.invalid", None, "owner", "repo").unwrap();
        let paths: Vec<String> = (0..50)
            .map(|i| format!("{}file-{i}.rs", "segment/".repeat(400)))
            .collect();
        let results =
            service.viewed_batch(&repository, "PR_1", &paths, true, &CancellationToken::new());
        assert!(
            results
                .iter()
                .all(|(_, result)| matches!(result, Ok(ReviewMutationOutcome::Applied(()))))
        );
        let commands = executor.commands.lock().unwrap();
        assert_eq!(commands.len(), 1);
        assert!(commands[0].arguments.iter().all(|arg| arg.len() < 65536));
        let payload: serde_json::Value = serde_json::from_slice(
            commands[0]
                .stdin
                .as_deref()
                .expect("large query goes on stdin"),
        )
        .unwrap();
        assert_eq!(payload["variables"]["pr"], "PR_1");
        assert_eq!(
            payload["query"]
                .as_str()
                .unwrap()
                .matches(":markFileAsViewed")
                .count(),
            50
        );
    }

    #[test]
    fn single_comment_rate_limit_pauses_the_shared_host_budget() {
        let executor = Arc::new(Recorder::default());
        let reset = polling::now() + 120;
        executor.outputs.lock().unwrap().push_back(ProcessOutput {
            success: false, exit_code: Some(1), signal: None,
            stdout: format!("HTTP/2.0 403 Forbidden\r\nRetry-After: 120\r\nX-RateLimit-Remaining: 4000\r\n\r\n{{\"message\":\"secondary rate limit\"}}").into_bytes(),
            stderr: vec![],
        });
        let service = GitHubService::new("gh", executor.clone());
        let key = PullRequestKey::new(
            GitHubRepoKey::new("single-limit-test.invalid", Some(8443), "owner", "repo").unwrap(),
            9,
        )
        .unwrap();
        let result = service.post_single_review_comment(
            &key,
            &"a".repeat(40),
            &AddReviewThreadRequest {
                review_id: "unused".into(),
                path: "file.rs".into(),
                subject: ReviewThreadSubject::File,
                body: "Visible now".into(),
            },
            &CancellationToken::new(),
        );
        assert!(matches!(
            result,
            Ok(ReviewMutationOutcome::NotApplied { .. })
        ));
        let limit = polling::rate_limit(&key.repository.host).unwrap();
        assert!(limit.retry_at >= reset && limit.retry_at <= reset + 1);
        assert!(!polling::ready(&key.repository.host, true));
        assert!(matches!(
            service.graphql(
                &key.repository,
                "query{viewer{login}}",
                vec![],
                &CancellationToken::new()
            ),
            Err(GitHubError::RateLimited { .. })
        ));
        let commands = executor.commands.lock().unwrap();
        assert_eq!(
            commands.len(),
            1,
            "the next review poll must not launch another process"
        );
        assert!(
            commands[0]
                .arguments
                .iter()
                .any(|arg| arg == "single-limit-test.invalid:8443")
        );
    }
    #[test]
    fn single_comments_publish_directly_without_review_id() {
        let executor = Arc::new(Recorder::default());
        executor
            .outputs
            .lock()
            .unwrap()
            .push_back(output(serde_json::json!({"id":42})));
        let service = GitHubService::new("gh", executor.clone());
        let key = PullRequestKey::new(
            GitHubRepoKey::new("single-test.invalid", None, "owner", "repo").unwrap(),
            9,
        )
        .unwrap();
        assert!(matches!(
            service.post_single_review_comment(
                &key,
                &"a".repeat(40),
                &AddReviewThreadRequest {
                    review_id: "unused".into(),
                    path: "file.rs".into(),
                    subject: ReviewThreadSubject::File,
                    body: "Visible now".into()
                },
                &CancellationToken::new()
            ),
            Ok(ReviewMutationOutcome::Applied(()))
        ));
        let commands = executor.commands.lock().unwrap();
        assert!(
            commands[0]
                .arguments
                .iter()
                .any(|arg| arg == "repos/owner/repo/pulls/9/comments")
        );
        assert!(
            !commands[0]
                .arguments
                .iter()
                .any(|arg| arg.to_string_lossy().contains("review_id"))
        );
    }
}
