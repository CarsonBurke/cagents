//! Interactive pull request actions shared by the native review surfaces.
use super::*;

impl<E: ProcessExecutor> GitHubService<E> {
    /// Search the repository, independently of the browser's loaded pages.
    pub fn search_pull_request_page(
        &self,
        repository: &GitHubRepoKey,
        text: &str,
        cursor: Option<&str>,
        cancellation: &CancellationToken,
    ) -> Result<PullRequestPage, GitHubError> {
        repository.validate()?;
        if let Some(number) = exact_number(text) {
            if cursor.is_some() {
                return Ok(PullRequestPage {
                    pull_requests: Vec::new(),
                    next_cursor: None,
                });
            }
            let query = format!(
                "query($owner:String!,$name:String!,$number:Int!){{repository(owner:$owner,name:$name){{pullRequest(number:$number){{{BROWSER_PULL_REQUEST_FIELDS}}}}}}}"
            );
            let data = self.graphql_allowing_not_found(
                repository,
                &query,
                vec![
                    ("owner", repository.owner.clone().into()),
                    ("name", repository.name.clone().into()),
                    ("number", number.into()),
                ],
                cancellation,
            )?;
            let value = data
                .pointer("/repository/pullRequest")
                .filter(|value| !value.is_null());
            let pull_requests = match value {
                Some(value)
                    if value.get("state").and_then(serde_json::Value::as_str) == Some("OPEN") =>
                {
                    vec![decode_browser_pull_request(value.clone(), repository)?]
                }
                _ => Vec::new(),
            };
            return Ok(PullRequestPage {
                pull_requests,
                next_cursor: None,
            });
        }
        let Some(search) = search_query(repository, text) else {
            // Nothing searchable remains, such as a query of only quotes.
            return Ok(PullRequestPage {
                pull_requests: Vec::new(),
                next_cursor: None,
            });
        };
        let query = format!(
            "query($searchText:String!,$cursor:String){{search(type:ISSUE,query:$searchText,first:50,after:$cursor){{nodes{{... on PullRequest{{{BROWSER_PULL_REQUEST_FIELDS}}}}} pageInfo{{hasNextPage endCursor}}}}}}"
        );
        let mut variables = vec![("searchText", search.into())];
        if let Some(cursor) = cursor {
            variables.push(("cursor", cursor.into()));
        }
        let data = self.graphql(repository, &query, variables, cancellation)?;
        let connection: Connection<serde_json::Value> =
            decode_value(data.get("search").ok_or_else(|| {
                GitHubError::Inspection("GitHub returned no search results".into())
            })?)?;
        let next = if connection.page_info.has_next_page {
            Some(next_cursor(&connection.page_info, cursor)?)
        } else {
            None
        };
        let pull_requests = connection
            .nodes
            .into_iter()
            .map(|value| decode_browser_pull_request(value, repository))
            .collect::<Result<Vec<_>, GitHubError>>()?;
        Ok(PullRequestPage {
            pull_requests,
            next_cursor: next,
        })
    }
}

/// Only an explicit `#123` names a pull request; a bare number such as a year
/// is ordinary title text.
fn exact_number(text: &str) -> Option<u64> {
    let number = text.trim().strip_prefix('#')?;
    (!number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| number.parse::<u64>().ok())
        .flatten()
        .filter(|number| *number > 0 && *number <= i32::MAX as u64)
}

/// Match the browser's explicit #number and @author affordances. Query terms
/// remain variables; user text cannot change the repository/open constraints.
/// `None` when no searchable text remains.
fn search_query(repository: &GitHubRepoKey, text: &str) -> Option<String> {
    let text = text.trim();
    // Creation order matches the open list, so a refreshed search keeps its
    // rows and pages in place instead of reordering by relevance.
    let scope = format!(
        "repo:{}/{} is:pr is:open sort:created-desc",
        repository.owner, repository.name
    );
    let author = text.strip_prefix('@').filter(|author| {
        !author.is_empty()
            && author
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    });
    if let Some(author) = author {
        return Some(format!("{scope} author:{author}"));
    }
    // GitHub search has no escape for a quote inside a phrase. Dropping
    // quotes keeps all user text inside the one title phrase.
    let phrase = text.replace('"', " ");
    let phrase = phrase.trim();
    (!phrase.is_empty()).then(|| format!("{scope} \"{phrase}\" in:title"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn repository_search_fetches_unloaded_pr_and_passes_query_and_cursor_as_variables() {
        struct SearchGit(std::sync::Mutex<Vec<CommandSpec>>);
        impl ProcessExecutor for SearchGit {
            fn execute(
                &self,
                command: &CommandSpec,
                _: &CancellationToken,
            ) -> Result<ProcessOutput, ProcessFailure> {
                self.0.lock().unwrap().push(command.clone());
                Ok(ProcessOutput { success: true, exit_code: Some(0), signal: None,
                    stdout: serde_json::json!({"data":{"search":{"nodes":[{
                        "number":912,"title":"needle beyond page one","state":"OPEN","headRefName":"topic",
                        "headRefOid":"abc","baseRefName":"main","updatedAt":"now",
                        "url":"https://github.com/owner/repo/pull/912",
                        "headRepository":{"name":"repo","owner":{"login":"owner"}},
                        "commits":{"nodes":[{"commit":{"statusCheckRollup":{"state":"FAILURE"}}}]}
                    }],"pageInfo":{"hasNextPage":true,"endCursor":"page3"}}}}).to_string().into_bytes(), stderr: Vec::new() })
            }
        }
        let executor = Arc::new(SearchGit(Default::default()));
        let service = GitHubService::new("gh", executor.clone());
        let repo = GitHubRepoKey::new("github.com", None, "owner", "repo").unwrap();
        let result = service
            .search_pull_request_page(&repo, "needle", Some("page2"), &CancellationToken::new())
            .unwrap();
        assert_eq!(result.pull_requests[0].number, 912);
        assert_eq!(result.pull_requests[0].head_repository_key(), Some(repo));
        assert_eq!(result.next_cursor.as_deref(), Some("page3"));
        // Search rows carry the same aggregate check state as loaded pages,
        // so check filters and row captions do not depend on how a row was found.
        let checks = result.pull_requests[0].check_summaries();
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].bucket.0, "fail");
        let calls = executor.0.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].arguments.iter().any(|arg| arg == "cursor=page2"));
        assert!(calls[0].arguments.iter().any(|arg| arg
            == "searchText=repo:owner/repo is:pr is:open sort:created-desc \"needle\" in:title"));
    }

    #[test]
    fn numeric_pull_request_search_uses_exact_repository_number_and_filters_closed() {
        struct Exact {
            calls: std::sync::Mutex<Vec<CommandSpec>>,
            state: &'static str,
        }
        impl ProcessExecutor for Exact {
            fn execute(
                &self,
                command: &CommandSpec,
                _: &CancellationToken,
            ) -> Result<ProcessOutput, ProcessFailure> {
                self.calls.lock().unwrap().push(command.clone());
                let response = serde_json::json!({"data":{"repository":{"pullRequest":{"number":912,"title":"Exact number","state":self.state,"headRefName":"feature","headRefOid":"a".repeat(40),"baseRefName":"main","updatedAt":"now","url":"https://exact-search.invalid/owner/repo/pull/912"}}}});
                Ok(ProcessOutput {
                    success: true,
                    exit_code: Some(0),
                    signal: None,
                    stdout: response.to_string().into_bytes(),
                    stderr: Vec::new(),
                })
            }
        }
        let repo = GitHubRepoKey::new("exact-search.invalid", None, "owner", "repo").unwrap();
        for (state, count) in [("OPEN", 1), ("MERGED", 0)] {
            let executor = Arc::new(Exact {
                calls: Default::default(),
                state,
            });
            let service = GitHubService::new("gh", executor.clone());
            let result = service
                .search_pull_request_page(&repo, "#912", None, &CancellationToken::new())
                .unwrap();
            assert_eq!(result.pull_requests.len(), count);
            assert!(result.next_cursor.is_none());
            let next = service
                .search_pull_request_page(
                    &repo,
                    "#912",
                    Some("obsolete-cursor"),
                    &CancellationToken::new(),
                )
                .unwrap();
            assert!(next.pull_requests.is_empty());
            let calls = executor.calls.lock().unwrap();
            assert_eq!(calls.len(), 1);
            assert!(
                calls[0]
                    .arguments
                    .iter()
                    .any(|argument| argument == "number=912")
            );
            assert!(calls[0].arguments.iter().any(|argument| {
                let argument = argument.to_string_lossy();
                argument.contains("pullRequest(number:$number)")
                    && argument.contains("statusCheckRollup{state}")
            }));
        }
    }

    #[test]
    fn missing_pull_request_number_is_an_empty_result_not_a_host_failure() {
        struct Missing;
        impl ProcessExecutor for Missing {
            fn execute(
                &self,
                _: &CommandSpec,
                _: &CancellationToken,
            ) -> Result<ProcessOutput, ProcessFailure> {
                // gh exits non-zero, but GitHub answered: no such pull request.
                let response = serde_json::json!({
                    "data": {"repository": {"pullRequest": null}},
                    "errors": [{
                        "type": "NOT_FOUND",
                        "path": ["repository", "pullRequest"],
                        "message": "Could not resolve to a PullRequest with the number of 2."
                    }]
                });
                Ok(ProcessOutput {
                    success: false,
                    exit_code: Some(1),
                    signal: None,
                    stdout: response.to_string().into_bytes(),
                    stderr: b"gh: Could not resolve to a PullRequest with the number of 2."
                        .to_vec(),
                })
            }
        }
        let repo = GitHubRepoKey::new("missing-number.invalid", None, "owner", "repo").unwrap();
        let service = GitHubService::new("gh", Arc::new(Missing));
        let result = service
            .search_pull_request_page(&repo, "#2", None, &CancellationToken::new())
            .unwrap();
        assert!(result.pull_requests.is_empty());
        assert!(result.next_cursor.is_none());
        assert!(
            crate::github::polling::ready(&repo.authority(), false),
            "an answered lookup must not pause polling for the host"
        );
    }

    #[test]
    fn search_scope_and_search_modes_are_explicit() {
        let repo = GitHubRepoKey::new("github.com", None, "owner", "repo").unwrap();
        assert_eq!(
            search_query(&repo, "@alice").as_deref(),
            Some("repo:owner/repo is:pr is:open sort:created-desc author:alice")
        );
        assert_eq!(exact_number("#1234"), Some(1234));
        assert_eq!(exact_number("1234"), None, "a bare number is title text");
        assert_eq!(exact_number("##1234"), None);
        assert_eq!(exact_number("#0"), None);
        assert_eq!(
            search_query(&repo, "fix sidebar").as_deref(),
            Some("repo:owner/repo is:pr is:open sort:created-desc \"fix sidebar\" in:title")
        );
        assert_eq!(
            search_query(&repo, "say \"hi\" repo:other/other").as_deref(),
            Some(
                "repo:owner/repo is:pr is:open sort:created-desc \"say  hi  repo:other/other\" in:title"
            ),
            "user text cannot close the phrase and add qualifiers"
        );
        assert_eq!(
            search_query(&repo, " \"\" "),
            None,
            "nothing left to search"
        );
    }

    #[test]
    fn missing_repository_is_a_failure_not_an_empty_search() {
        struct Gone;
        impl ProcessExecutor for Gone {
            fn execute(
                &self,
                _: &CommandSpec,
                _: &CancellationToken,
            ) -> Result<ProcessOutput, ProcessFailure> {
                let response = serde_json::json!({
                    "data": {"repository": null},
                    "errors": [{
                        "type": "NOT_FOUND",
                        "path": ["repository"],
                        "message": "Could not resolve to a Repository with the name 'owner/repo'."
                    }]
                });
                Ok(ProcessOutput {
                    success: false,
                    exit_code: Some(1),
                    signal: None,
                    stdout: response.to_string().into_bytes(),
                    stderr: b"gh: Could not resolve to a Repository".to_vec(),
                })
            }
        }
        let repo = GitHubRepoKey::new("missing-repository.invalid", None, "owner", "repo").unwrap();
        let service = GitHubService::new("gh", Arc::new(Gone));
        assert!(
            service
                .search_pull_request_page(&repo, "#2", None, &CancellationToken::new())
                .is_err(),
            "a renamed or inaccessible repository is not an empty result"
        );
        crate::github::polling::succeeded(&repo.authority());
    }
}
