//! GitHub Actions operations and bounded context prepared for an editable chat draft.
use super::*;

pub const LOG_LINES_PER_STEP: usize = 200;
const LOG_DOWNLOAD_BYTES: usize = 8 * 1024 * 1024;
const PROMPT_LOG_BYTES: usize = 128 * 1024;
const MAX_THREAD_PROMPT_BYTES: usize = 2 * 1024 * 1024;

/// Third-party CI links never become GitHub Actions commands.
pub fn workflow_run(repository: &GitHubRepoKey, link: &Url) -> Option<u64> {
    if link.scheme() != "https"
        || link.host_str()? != repository.host
        || link.port_or_known_default() != repository.port.or(Some(443))
        || link.username() != ""
        || link.password().is_some()
    {
        return None;
    }
    let parts: Vec<_> = link.path_segments()?.collect();
    if parts.len() < 5
        || !parts[0].eq_ignore_ascii_case(&repository.owner)
        || !parts[1].eq_ignore_ascii_case(&repository.name)
        || parts[2] != "actions"
        || parts[3] != "runs"
    {
        return None;
    }
    parts[4].parse::<u64>().ok().filter(|id| *id > 0)
}

impl<E: ProcessExecutor> GitHubService<E> {
    pub fn rerun_failed_workflow(
        &self,
        repository: &GitHubRepoKey,
        run: u64,
        cancellation: &CancellationToken,
    ) -> Result<(), GitHubError> {
        repository.validate()?;
        let command = self.gh_command([
            "run".into(),
            "rerun".into(),
            run.to_string(),
            "--failed".into(),
            "--repo".into(),
            repository.gh_selector(),
        ]);
        self.run_checked(&command, repository, cancellation)?;
        Ok(())
    }

    pub fn failed_workflow_logs(
        &self,
        repository: &GitHubRepoKey,
        run: u64,
        cancellation: &CancellationToken,
    ) -> Result<String, GitHubError> {
        repository.validate()?;
        let command = self
            .gh_command([
                "run".into(),
                "view".into(),
                run.to_string(),
                "--log-failed".into(),
                "--repo".into(),
                repository.gh_selector(),
            ])
            .output_limits(LOG_DOWNLOAD_BYTES, 64 * 1024);
        let output = self.run_checked(&command, repository, cancellation)?;
        Ok(failed_step_tails(
            &String::from_utf8_lossy(&output.stdout),
            LOG_LINES_PER_STEP,
        ))
    }

    pub fn review_thread_hunks(
        &self,
        repository: &GitHubRepoKey,
        threads: &[String],
        cancellation: &CancellationToken,
    ) -> Result<Vec<String>, GitHubError> {
        repository.validate()?;
        let mut result = Vec::with_capacity(threads.len());
        for chunk in threads.chunks(50) {
            let names: Vec<_> = (0..chunk.len())
                .map(|index| format!("thread{index}"))
                .collect();
            let mut query = format!(
                "query({}){{",
                names
                    .iter()
                    .map(|name| format!("${name}:ID!"))
                    .collect::<Vec<_>>()
                    .join(",")
            );
            for (index, name) in names.iter().enumerate() {
                query.push_str(&format!("t{index}:node(id:${name}){{... on PullRequestReviewThread{{comments(first:1){{nodes{{diffHunk}}}}}}}}"));
            }
            query.push('}');
            let variables = names
                .iter()
                .zip(chunk)
                .map(|(name, id)| (name.as_str(), id.as_str().into()))
                .collect();
            let data = self.graphql(repository, &query, variables, cancellation)?;
            for index in 0..chunk.len() {
                result.push(
                    data.pointer(&format!("/t{index}/comments/nodes/0/diffHunk"))
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("(diff hunk unavailable)")
                        .to_owned(),
                );
            }
        }
        Ok(result)
    }

    /// Ordinary inspector reads cap long threads. A deliberate agent handoff
    /// completes only those selected threads, with a separate context budget.
    pub fn complete_review_thread_comments(
        &self,
        repository: &GitHubRepoKey,
        mut thread: PullRequestReviewThread,
        cancellation: &CancellationToken,
    ) -> Result<PullRequestReviewThread, GitHubError> {
        repository.validate()?;
        validate_node_id("thread_id", &thread.id)?;
        let query = format!(
            "query($id:ID!,$cursor:String){{node(id:$id){{... on PullRequestReviewThread{{comments(first:50,after:$cursor){{nodes{{{REVIEW_COMMENT_FIELDS}}} pageInfo{{hasNextPage endCursor}}}}}}}}}}"
        );
        let mut cursor: Option<String> = None;
        let mut comments = Vec::new();
        let mut bytes = 0usize;
        for _ in 0..40 {
            let mut variables = vec![("id", thread.id.as_str().into())];
            if let Some(cursor) = &cursor {
                variables.push(("cursor", cursor.as_str().into()));
            }
            let data = self.graphql(repository, &query, variables, cancellation)?;
            let connection = data.pointer("/node/comments").ok_or_else(|| {
                GitHubError::Inspection("The selected review thread is no longer available.".into())
            })?;
            let page: Connection<PullRequestComment> = decode_value(connection)?;
            for comment in page.nodes {
                bytes = bytes
                    .saturating_add(comment.body.len())
                    .saturating_add(comment.url.as_str().len())
                    .saturating_add(256);
                if bytes > MAX_THREAD_PROMPT_BYTES {
                    return Err(GitHubError::Inspection("The selected thread exceeds the 2 MiB agent context limit. Open its full discussion on GitHub.".into()));
                }
                comments.push(comment);
            }
            if !page.page_info.has_next_page {
                thread.comments = comments;
                thread.truncated = false;
                return Ok(thread);
            }
            cursor = Some(next_cursor(&page.page_info, cursor.as_deref())?);
        }
        Err(GitHubError::Inspection("The selected thread has more than 2,000 comments. Open its full discussion on GitHub instead of sending partial context.".into()))
    }
}

/// `gh run view` prefixes each line with job and step separated by tabs.
/// Preserve encounter order and the tail of every failed step, with a final
/// byte cap so a large or malformed log cannot fill an agent's context.
pub fn failed_step_tails(log: &str, per_step: usize) -> String {
    let mut steps: Vec<(String, VecDeque<&str>)> = Vec::new();
    let mut indices = BTreeMap::new();
    for line in log.lines() {
        let mut fields = line.splitn(3, '\t');
        let job = fields.next().unwrap_or_default();
        let step = fields.next();
        let key = step.map_or_else(
            || "Raw log output".to_owned(),
            |step| format!("{job}\t{step}"),
        );
        let key = if indices.len() < 512 || indices.contains_key(&key) {
            key
        } else {
            "Additional failed steps".into()
        };
        let index = *indices.entry(key.clone()).or_insert_with(|| {
            steps.push((key, VecDeque::new()));
            steps.len() - 1
        });
        let lines = &mut steps[index].1;
        lines.push_back(line);
        while lines.len() > per_step.max(1) {
            lines.pop_front();
        }
    }
    let mut result = steps
        .into_iter()
        .flat_map(|(_, lines)| lines)
        .collect::<Vec<_>>()
        .join("\n");
    if result.len() > PROMPT_LOG_BYTES {
        let mut start = result.len() - PROMPT_LOG_BYTES;
        while !result.is_char_boundary(start) {
            start += 1;
        }
        result = format!("[Earlier log text omitted]\n{}", &result[start..]);
    }
    result
}

pub fn failed_check_prompt(pr: &PullRequest, check: &PullRequestCheck, logs: &str) -> String {
    format!(
        "Fix the failing GitHub Actions check for PR #{}: {}\n{}\nBranch: {}\nHead: {}\nCheck: {}\nRun: {}\n\nFailed step log tails (up to {} lines per step):\n\n{}\n\nInvestigate and fix the failure, run the relevant checks, and summarize the change.\n",
        pr.number,
        pr.title,
        pr.url,
        pr.head_ref_name,
        pr.head_ref_oid,
        check.name,
        check.link,
        LOG_LINES_PER_STEP,
        logs
    )
}

pub fn review_threads_prompt(
    pr: &PullRequest,
    threads: &[(PullRequestReviewThread, String)],
) -> Result<String, String> {
    let mut prompt = format!(
        "Address the selected review threads on PR #{}: {}\n{}\nBranch: {}\nHead: {}\n",
        pr.number, pr.title, pr.url, pr.head_ref_name, pr.head_ref_oid
    );
    for (thread, hunk) in threads {
        if thread.truncated {
            return Err("The selected thread is incomplete. Refresh its full discussion before sending it to an agent.".into());
        }
        prompt.push_str(&format!(
            "\nFile: {}\nLines: {}..{} ({:?})\nThread: {}\nDiff hunk:\n{}\nComments:\n",
            thread.path,
            thread
                .start_line
                .or(thread.line)
                .map_or("file".into(), |n| n.to_string()),
            thread.line.map_or("file".into(), |n| n.to_string()),
            thread.side,
            thread.id,
            hunk
        ));
        if prompt.len() > MAX_THREAD_PROMPT_BYTES {
            return Err("The selected review context exceeds 2 MiB. Select fewer threads to include their complete context.".into());
        }
        for comment in &thread.comments {
            prompt.push_str(&format!(
                "\n{} ({}):\n{}\n",
                comment
                    .author
                    .as_ref()
                    .map_or("Deleted account", |a| a.login.as_str()),
                comment.url,
                comment.body
            ));
            if prompt.len() > MAX_THREAD_PROMPT_BYTES {
                return Err("The selected review context exceeds 2 MiB. Select fewer threads to include their complete context.".into());
            }
        }
    }
    prompt.push_str("\nMake the requested changes, verify them, and summarize how each thread was addressed. Leave thread resolution to the reviewer.\n");
    Ok(prompt)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn pull_request() -> PullRequest {
        serde_json::from_value(serde_json::json!({"number":7,"title":"Fix CI","state":"OPEN","headRefName":"feature/x","headRefOid":"abc","baseRefName":"main","updatedAt":"now","url":"https://github.com/owner/repo/pull/7"})).unwrap()
    }
    #[test]
    fn selected_threads_include_each_location_hunk_author_and_comment_in_one_prompt() {
        let thread = |id: &str, path: &str, line: u64, body: &str| {
            PullRequestReviewThread {
            id: id.into(), path: path.into(), line: Some(line), start_line: Some(line-1), side: ReviewDiffSide::Right,
            start_side: Some(ReviewDiffSide::Right), outdated: false, resolved: false, collapsed: false,
            viewer_can_resolve: false, viewer_can_unresolve: false, truncated: false,
            comments: vec![serde_json::from_value(serde_json::json!({"id":id,"author":{"login":"reviewer"},"body":body,"createdAt":"now","url":"https://github.com/owner/repo/pull/7#discussion_r1"})).unwrap()],
        }
        };
        let prompt = review_threads_prompt(
            &pull_request(),
            &[
                (
                    thread("one", "src/one.rs", 10, "Keep the first guard"),
                    "@@ -9,2 +9,2 @@ first hunk".into(),
                ),
                (
                    thread("two", "src/two.rs", 20, "Fix the second branch"),
                    "@@ -19,2 +19,2 @@ second hunk".into(),
                ),
            ],
        )
        .unwrap();
        for text in [
            "src/one.rs",
            "src/two.rs",
            "9..10",
            "19..20",
            "first hunk",
            "second hunk",
            "reviewer",
            "Keep the first guard",
            "Fix the second branch",
            "feature/x",
        ] {
            assert!(prompt.contains(text), "missing {text}");
        }
    }
    #[test]
    fn actions_requests_rerun_only_failed_jobs_and_bound_log_download() {
        struct Fake(std::sync::Mutex<Vec<CommandSpec>>);
        impl ProcessExecutor for Fake {
            fn execute(
                &self,
                command: &CommandSpec,
                _: &CancellationToken,
            ) -> Result<ProcessOutput, ProcessFailure> {
                self.0.lock().unwrap().push(command.clone());
                Ok(ProcessOutput {
                    success: true,
                    exit_code: Some(0),
                    signal: None,
                    stdout: b"job\tstep\tfailed line".to_vec(),
                    stderr: Vec::new(),
                })
            }
        }
        let fake = Arc::new(Fake(Default::default()));
        let service = GitHubService::new("gh", fake.clone());
        let repo = GitHubRepoKey::new("github.com", None, "owner", "repo").unwrap();
        service
            .rerun_failed_workflow(&repo, 123, &CancellationToken::new())
            .unwrap();
        let logs = service
            .failed_workflow_logs(&repo, 123, &CancellationToken::new())
            .unwrap();
        assert!(logs.contains("failed line"));
        let commands = fake.0.lock().unwrap();
        assert_eq!(
            commands[0].arguments,
            [
                "run",
                "rerun",
                "123",
                "--failed",
                "--repo",
                "github.com/owner/repo"
            ]
            .map(OsString::from)
        );
        assert!(
            commands[1]
                .arguments
                .iter()
                .any(|arg| arg == "--log-failed")
        );
        assert_eq!(commands[1].stdout_limit, LOG_DOWNLOAD_BYTES);
    }
    #[test]
    fn retains_last_lines_of_each_step_and_caps_utf8_log_size() {
        let log = "job\tfirst\told\njob\tfirst\tnew\njob\tsecond\tother";
        let tail = failed_step_tails(log, 1);
        assert!(!tail.contains("old"));
        assert!(tail.contains("new"));
        assert!(tail.contains("other"));
        assert!(
            failed_step_tails(&"é".repeat(PROMPT_LOG_BYTES), 200).len() < PROMPT_LOG_BYTES + 100
        );
    }
    #[test]
    fn only_the_repositorys_actions_runs_are_actionable() {
        let repo = GitHubRepoKey::new("github.com", None, "owner", "repo").unwrap();
        assert_eq!(
            workflow_run(
                &repo,
                &"https://github.com/owner/repo/actions/runs/123/job/456"
                    .parse()
                    .unwrap()
            ),
            Some(123)
        );
        assert_eq!(
            workflow_run(
                &repo,
                &"https://github.com/other/repo/actions/runs/123"
                    .parse()
                    .unwrap()
            ),
            None
        );
        assert_eq!(
            workflow_run(
                &repo,
                &"https://ci.example/owner/repo/actions/runs/123"
                    .parse()
                    .unwrap()
            ),
            None
        );
    }
}

#[cfg(test)]
mod batch_tests {
    use super::*;
    #[test]
    fn many_thread_hunks_are_batched_with_typed_id_variables() {
        struct Fake(Mutex<Vec<CommandSpec>>);
        impl ProcessExecutor for Fake {
            fn execute(
                &self,
                command: &CommandSpec,
                _: &CancellationToken,
            ) -> Result<ProcessOutput, ProcessFailure> {
                self.0.lock().unwrap().push(command.clone());
                let mut data = serde_json::Map::new();
                for index in 0..50 {
                    data.insert(format!("t{index}"),serde_json::json!({"comments":{"nodes":[{"diffHunk":format!("hunk-{index}")}]}}));
                }
                Ok(ProcessOutput {
                    success: true,
                    exit_code: Some(0),
                    signal: None,
                    stdout: serde_json::json!({"data":data}).to_string().into_bytes(),
                    stderr: Vec::new(),
                })
            }
        }
        let executor = Arc::new(Fake(Default::default()));
        let service = GitHubService::new("gh", executor.clone());
        let repo = GitHubRepoKey::new("batch-context.invalid", None, "owner", "repo").unwrap();
        let ids = (0..61)
            .map(|index| format!("thread-id-{index}"))
            .collect::<Vec<_>>();
        let result = service
            .review_thread_hunks(&repo, &ids, &CancellationToken::new())
            .unwrap();
        assert_eq!(result.len(), 61);
        assert_eq!(result[50], "hunk-0");
        let calls = executor.0.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert!(
            calls[1]
                .arguments
                .iter()
                .any(|argument| argument == "thread0=thread-id-50")
        );
        assert!(
            !calls[0]
                .arguments
                .iter()
                .find(|argument| argument.to_string_lossy().starts_with("query="))
                .unwrap()
                .to_string_lossy()
                .contains("thread-id-0")
        );
    }

    struct LongThread {
        calls: Mutex<Vec<CommandSpec>>,
        oversized: bool,
    }
    impl ProcessExecutor for LongThread {
        fn execute(
            &self,
            command: &CommandSpec,
            _: &CancellationToken,
        ) -> Result<ProcessOutput, ProcessFailure> {
            let mut calls = self.calls.lock().unwrap();
            let page = calls.len();
            calls.push(command.clone());
            let range = if self.oversized {
                0..1
            } else {
                match page {
                    0 => 0..50,
                    1 => 50..100,
                    _ => 100..101,
                }
            };
            let nodes: Vec<_> = range.map(|index| serde_json::json!({"id":format!("comment-{index}"),"body":if self.oversized {"x".repeat(MAX_THREAD_PROMPT_BYTES + 1)} else {format!("Complete comment {index}")},"createdAt":"now","url":format!("https://github.com/owner/repo/pull/7#discussion_r{index}"),"diffHunk":"@@ -1 +1 @@ full context","commentState":"SUBMITTED"})).collect();
            let response = serde_json::json!({"data":{"node":{"comments":{"nodes":nodes,"pageInfo":{"hasNextPage":page<2,"endCursor":format!("page{}",page+1)}}}}});
            Ok(ProcessOutput {
                success: true,
                exit_code: Some(0),
                signal: None,
                stdout: response.to_string().into_bytes(),
                stderr: Vec::new(),
            })
        }
    }
    fn truncated_thread() -> PullRequestReviewThread {
        let page: ReviewThreadPage = serde_json::from_value(serde_json::json!({"id":"thread","path":"src/file.rs","line":1,"diffSide":"RIGHT","isOutdated":false,"isResolved":false,"comments":{"nodes":[],"pageInfo":{"hasNextPage":true,"endCursor":"cached"}}})).unwrap();
        page.into_thread(Vec::new(), true)
    }
    #[test]
    fn agent_handoff_completes_a_selected_thread_beyond_the_inspector_comment_cap() {
        let executor = Arc::new(LongThread {
            calls: Mutex::new(Vec::new()),
            oversized: false,
        });
        let service = GitHubService::new("gh", executor.clone());
        let repository =
            GitHubRepoKey::new("thread-context.invalid", None, "owner", "repo").unwrap();
        let thread = service
            .complete_review_thread_comments(
                &repository,
                truncated_thread(),
                &CancellationToken::new(),
            )
            .unwrap();
        assert_eq!(thread.comments.len(), 101);
        assert!(!thread.truncated);
        assert_eq!(thread.comments[100].body, "Complete comment 100");
        let calls = executor.calls.lock().unwrap();
        assert_eq!(calls.len(), 3);
        assert!(calls[1].arguments.iter().any(|arg| arg == "cursor=page1"));
        assert!(calls[2].arguments.iter().any(|arg| arg == "cursor=page2"));
    }
    #[test]
    fn agent_handoff_refuses_oversized_or_incomplete_thread_context() {
        let executor = Arc::new(LongThread {
            calls: Mutex::new(Vec::new()),
            oversized: true,
        });
        let service = GitHubService::new("gh", executor.clone());
        let repository =
            GitHubRepoKey::new("thread-budget.invalid", None, "owner", "repo").unwrap();
        assert!(
            service
                .complete_review_thread_comments(
                    &repository,
                    truncated_thread(),
                    &CancellationToken::new()
                )
                .unwrap_err()
                .to_string()
                .contains("2 MiB")
        );
        assert_eq!(executor.calls.lock().unwrap().len(), 1);
        let pr: PullRequest = serde_json::from_value(serde_json::json!({"number":7,"title":"Review","state":"OPEN","headRefName":"feature","headRefOid":"abc","baseRefName":"main","updatedAt":"now","url":"https://github.com/owner/repo/pull/7"})).unwrap();
        assert!(
            review_threads_prompt(&pr, &[(truncated_thread(), "hunk".into())])
                .unwrap_err()
                .contains("incomplete")
        );
    }
}
