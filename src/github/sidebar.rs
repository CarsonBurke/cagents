//! Sidebar history needs identity and terminal state, not live check trees.
use super::*;

const HISTORY_FIELDS: &str = "id number title state isDraft headRefName headRefOid baseRefName baseRefOid author{login} updatedAt url headRepository{name owner{login}}";

impl<E: ProcessExecutor> GitHubService<E> {
    /// The sidebar shows aggregate check state. Individual check runs are
    /// loaded by the inspector when the person opens the pull request.
    pub fn sidebar_open(
        &self,
        repository: &GitHubRepoKey,
        limit: u16,
        cancellation: &CancellationToken,
    ) -> Result<Vec<PullRequest>, GitHubError> {
        self.sidebar_page(repository, limit, false, cancellation)
    }

    fn sidebar_page(
        &self,
        repository: &GitHubRepoKey,
        limit: u16,
        history: bool,
        cancellation: &CancellationToken,
    ) -> Result<Vec<PullRequest>, GitHubError> {
        let (operation, state, fields) = if history {
            ("SidebarHistory", "", HISTORY_FIELDS)
        } else {
            (
                "SidebarPullRequests",
                "states:OPEN,",
                BROWSER_PULL_REQUEST_FIELDS,
            )
        };
        let query = format!(
            "query {operation}($owner:String!,$name:String!,$limit:Int!){{rateLimit{{cost}} repository(owner:$owner,name:$name){{pullRequests(first:$limit,{state}orderBy:{{field:CREATED_AT,direction:DESC}}){{nodes{{{fields}}}}}}}}}"
        );
        let data = self.graphql(
            repository,
            &query,
            vec![
                ("owner", repository.owner.as_str().into()),
                ("name", repository.name.as_str().into()),
                ("limit", GraphQlValue::Int(limit.clamp(1, 100).into())),
            ],
            cancellation,
        )?;
        let nodes = data
            .pointer("/repository/pullRequests/nodes")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| {
                GitHubError::Inspection("GitHub returned no sidebar pull requests".into())
            })?;
        nodes
            .iter()
            .cloned()
            .map(|node| decode_browser_pull_request(node, repository))
            .collect()
    }

    /// Keep the same recent-history membership as the full listing, while
    /// excluding expensive checks, labels and mergeability calculations.
    /// Open rows from this read must not replace the full open-page rows.
    pub fn recent_history(
        &self,
        repository: &GitHubRepoKey,
        limit: u16,
        cancellation: &CancellationToken,
    ) -> Result<Vec<PullRequest>, GitHubError> {
        self.sidebar_page(repository, limit, true, cancellation)
    }
}

/// The open page owns live checks. A history read also contains open rows, so
/// replacing duplicates would erase their check and review state.
pub fn append_recent_history(open: &mut Vec<PullRequest>, history: Vec<PullRequest>) {
    for entry in history {
        if !open.iter().any(|current| current.number == entry.number) {
            open.push(entry);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_keeps_the_same_page_without_live_check_connections() {
        #[derive(Default)]
        struct Empty(Mutex<Vec<CommandSpec>>);
        impl ProcessExecutor for Empty {
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
                    stdout: br#"{"data":{"repository":{"pullRequests":{"nodes":[]}}}}"#.to_vec(),
                    stderr: vec![],
                })
            }
        }
        let repository = GitHubRepoKey::new("github.com", None, "owner", "repo").unwrap();
        let executor = Arc::new(Empty::default());
        let service = GitHubService::new("gh", executor.clone());
        let cancellation = CancellationToken::new();
        assert!(
            service
                .recent_history(&repository, 50, &cancellation)
                .unwrap()
                .is_empty()
        );
        assert!(
            service
                .sidebar_open(&repository, 50, &cancellation)
                .unwrap()
                .is_empty()
        );
        let calls = executor.0.lock().unwrap();
        assert_eq!(
            calls.len(),
            2,
            "one request per page, with no gh CLI discovery queries"
        );
        let query = |command: &CommandSpec| {
            command
                .arguments
                .iter()
                .find_map(|arg| arg.to_str().and_then(|s| s.strip_prefix("query=")))
                .unwrap()
                .to_owned()
        };
        let history = query(&calls[0]);
        assert!(history.contains("field:CREATED_AT,direction:DESC"));
        assert!(!history.contains("states:OPEN"));
        for expensive in [
            "statusCheckRollup",
            "mergeable",
            "mergeStateStatus",
            "labels",
        ] {
            assert!(!history.contains(expensive));
        }
        let open = query(&calls[1]);
        assert!(open.contains("states:OPEN"));
        assert!(open.contains("statusCheckRollup{state}"));
        assert!(!open.contains("contexts("));
        assert!(open.contains("rateLimit{cost}"));
    }

    #[test]
    fn history_never_erases_live_check_state() {
        let repository = GitHubRepoKey::new("github.com", None, "owner", "repo").unwrap();
        let live = parse_pull_request(br#"{"number":7,"title":"Live","state":"OPEN","headRefName":"feature","baseRefName":"main","updatedAt":"now","url":"https://github.com/owner/repo/pull/7","statusCheckRollup":[{"__typename":"StatusContext","context":"ci","state":"FAILURE"}]}"#, &repository).unwrap();
        let history = parse_pull_request(br#"{"number":7,"title":"History","state":"OPEN","headRefName":"feature","baseRefName":"main","updatedAt":"now","url":"https://github.com/owner/repo/pull/7"}"#, &repository).unwrap();
        let closed = parse_pull_request(br#"{"number":8,"title":"Closed","state":"MERGED","headRefName":"old","baseRefName":"main","updatedAt":"now","url":"https://github.com/owner/repo/pull/8"}"#, &repository).unwrap();
        let mut rows = vec![live.clone()];
        append_recent_history(&mut rows, vec![history, closed.clone()]);
        assert_eq!(rows, vec![live, closed]);
        assert!(!rows[0].check_summaries().is_empty());
    }
}
