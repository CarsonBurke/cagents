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
            let query = "query($owner:String!,$name:String!,$number:Int!){repository(owner:$owner,name:$name){pullRequest(number:$number){id number title state isDraft headRefName headRefOid baseRefName baseRefOid author{login} updatedAt url reviewDecision headRepository{name owner{login}}}}}";
            let data = self.graphql(
                repository,
                query,
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
                    vec![decode_search_pr(value.clone(), repository)?]
                }
                _ => Vec::new(),
            };
            return Ok(PullRequestPage {
                pull_requests,
                next_cursor: None,
            });
        }
        let search = search_query(repository, text);
        let query = "query($searchText:String!,$cursor:String){search(type:ISSUE,query:$searchText,first:50,after:$cursor){nodes{... on PullRequest{id number title state isDraft headRefName headRefOid baseRefName baseRefOid author{login} updatedAt url reviewDecision headRepository{name owner{login}}}} pageInfo{hasNextPage endCursor}}}";
        let mut variables = vec![("searchText", search.into())];
        if let Some(cursor) = cursor {
            variables.push(("cursor", cursor.into()));
        }
        let data = self.graphql(repository, query, variables, cancellation)?;
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
            .map(|value| decode_search_pr(value, repository))
            .collect::<Result<Vec<_>, GitHubError>>()?;
        Ok(PullRequestPage {
            pull_requests,
            next_cursor: next,
        })
    }
}

fn exact_number(text: &str) -> Option<u64> {
    let number = text.trim().strip_prefix('#').unwrap_or(text.trim());
    (!number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| number.parse::<u64>().ok())
        .flatten()
        .filter(|number| *number > 0 && *number <= i32::MAX as u64)
}

fn decode_search_pr(
    mut value: serde_json::Value,
    repository: &GitHubRepoKey,
) -> Result<PullRequest, GitHubError> {
    value["headRepositoryOwner"] = value
        .pointer("/headRepository/owner")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let pr: PullRequest = decode_value(&value)?;
    validate_pull_request(&pr, repository)?;
    Ok(pr)
}

/// Match the browser's explicit #number and @author affordances. Query terms
/// remain variables; user text cannot change the repository/open constraints.
fn search_query(repository: &GitHubRepoKey, text: &str) -> String {
    let text = text.trim();
    let scope = format!(
        "repo:{}/{} is:pr is:open",
        repository.owner, repository.name
    );
    let author = text.strip_prefix('@').filter(|author| {
        !author.is_empty()
            && author
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    });
    if let Some(author) = author {
        return format!("{scope} author:{author}");
    }
    let quoted = text.replace('\\', "\\\\").replace('"', "\\\"");
    format!("{scope} \"{quoted}\" in:title")
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
                        "headRepository":{"name":"repo","owner":{"login":"owner"}}
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
        let calls = executor.0.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].arguments.iter().any(|arg| arg == "cursor=page2"));
        assert!(
            calls[0]
                .arguments
                .iter()
                .any(|arg| arg == "searchText=repo:owner/repo is:pr is:open \"needle\" in:title")
        );
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
                argument
                    .to_string_lossy()
                    .contains("pullRequest(number:$number)")
            }));
        }
    }

    #[test]
    fn search_scope_and_search_modes_are_explicit() {
        let repo = GitHubRepoKey::new("github.com", None, "owner", "repo").unwrap();
        assert_eq!(
            search_query(&repo, "@alice"),
            "repo:owner/repo is:pr is:open author:alice"
        );
        assert_eq!(exact_number("#1234"), Some(1234));
        assert_eq!(exact_number("1234"), Some(1234));
        assert_eq!(exact_number("##1234"), None);
        assert_eq!(exact_number("0"), None);
        assert_eq!(
            search_query(&repo, "fix sidebar"),
            "repo:owner/repo is:pr is:open \"fix sidebar\" in:title"
        );
        assert!(!search_query(&repo, "fix repo:other/other").contains(" OR "));
    }
}
