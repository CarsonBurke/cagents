//! The Work board's listing of the user's own pull requests: one GraphQL
//! document holding both searches and the viewer, where `gh pr list` needed a
//! process and a request for each.
use super::*;

const IDENTITY: &str = "number title state isDraft headRefName headRefOid headRepository{name} headRepositoryOwner{login} baseRefName updatedAt url";
/// Review and check state, which GitHub computes per pull request (a hundred
/// rollups can time out a query), so it is asked for open ones alone.
const REVIEW: &str = "reviewDecision additions deletions commits(last:1){nodes{commit{statusCheckRollup{contexts(first:100){nodes{__typename ...on CheckRun{name status conclusion detailsUrl startedAt completedAt checkSuite{workflowRun{workflow{name}}}} ...on StatusContext{context state targetUrl createdAt}}}}}}}";

/// The user's pull requests in one repository, newest first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthoredPullRequests {
    /// Who `@me` is, for asking about the same pull requests over REST.
    pub viewer: String,
    /// Open ones, with review and check state.
    pub open: Vec<PullRequest>,
    /// The most recently updated in any state, identity and state only.
    pub all: Vec<PullRequest>,
}

impl<E: ProcessExecutor> GitHubService<E> {
    pub fn authored_pull_requests(
        &self,
        repository: &GitHubRepoKey,
        open: u16,
        all: u16,
        cancellation: &CancellationToken,
    ) -> Result<AuthoredPullRequests, GitHubError> {
        repository.validate()?;
        let query = format!(
            "query CagentsAuthoredPullRequests($open:String!,$all:String!,$openLimit:Int!,$allLimit:Int!){{viewer{{login}} open:search(query:$open,type:ISSUE,first:$openLimit){{nodes{{...on PullRequest{{{IDENTITY} {REVIEW}}}}}}} all:search(query:$all,type:ISSUE,first:$allLimit){{nodes{{...on PullRequest{{{IDENTITY}}}}}}}}}"
        );
        let search = format!(
            "repo:{}/{} is:pr author:@me sort:updated-desc",
            repository.owner, repository.name
        );
        let data = self.graphql(
            repository,
            &query,
            vec![
                ("open", format!("{search} is:open").into()),
                ("all", search.into()),
                ("openLimit", GraphQlValue::Int(open.clamp(1, 100).into())),
                ("allLimit", GraphQlValue::Int(all.clamp(1, 100).into())),
            ],
            cancellation,
        )?;
        let viewer = data
            .pointer("/viewer/login")
            .and_then(serde_json::Value::as_str)
            .filter(|login| !login.is_empty())
            .ok_or_else(|| GitHubError::Inspection("GitHub did not say who is signed in".into()))?
            .to_owned();
        Ok(AuthoredPullRequests {
            viewer,
            open: decode_authored(&data, "open", repository)?,
            all: decode_authored(&data, "all", repository)?,
        })
    }
}

fn decode_authored(
    data: &serde_json::Value,
    search: &str,
    repository: &GitHubRepoKey,
) -> Result<Vec<PullRequest>, GitHubError> {
    let review = search == "open";
    data.pointer(&format!("/{search}/nodes"))
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| {
            GitHubError::Inspection(format!("GitHub returned no {search} pull request search"))
        })?
        .iter()
        // A search result that is not a pull request selects no fields.
        .filter(|node| node.get("number").is_some())
        .cloned()
        .map(|mut node| {
            if review {
                // gh's shape: a pull request with no checks has an empty
                // rollup, which the board reads as known, not unknown.
                let mut contexts = node
                    .pointer_mut("/commits/nodes/0/commit/statusCheckRollup/contexts/nodes")
                    .map(serde_json::Value::take)
                    .unwrap_or_else(|| serde_json::json!([]));
                // gh's shape names a check run's workflow beside it.
                for context in contexts.as_array_mut().into_iter().flatten() {
                    let Some(suite) = context
                        .as_object_mut()
                        .and_then(|context| context.remove("checkSuite"))
                    else {
                        continue;
                    };
                    if let Some(workflow) = suite.pointer("/workflowRun/workflow/name") {
                        context["workflowName"] = workflow.clone();
                    }
                }
                node["statusCheckRollup"] = contexts;
            }
            if let Some(object) = node.as_object_mut() {
                object.remove("commits");
            }
            let pull_request: PullRequest = decode_value(&node)?;
            validate_pull_request(&pull_request, repository)?;
            Ok(pull_request)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Answer(Mutex<Vec<CommandSpec>>, String);

    impl ProcessExecutor for Answer {
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
                stdout: format!(
                    "HTTP/2.0 200 OK\r\nX-Ratelimit-Remaining: 4000\r\n\r\n{}",
                    self.1
                )
                .into_bytes(),
                stderr: vec![],
            })
        }
    }

    fn node(number: u64, state: &str) -> serde_json::Value {
        serde_json::json!({
            "number": number,
            "title": format!("PR {number}"),
            "state": state,
            "isDraft": false,
            "headRefName": format!("branch-{number}"),
            "headRefOid": "a".repeat(40),
            "headRepository": {"name": "repo"},
            "headRepositoryOwner": {"login": "owner"},
            "baseRefName": "main",
            "updatedAt": "2026-11-12T00:00:00Z",
            "url": format!("https://github.com/owner/repo/pull/{number}"),
        })
    }

    #[test]
    fn one_document_reads_both_searches_and_the_viewer() {
        let mut checked = node(2, "OPEN");
        checked["reviewDecision"] = "APPROVED".into();
        checked["additions"] = 7.into();
        checked["commits"] = serde_json::json!({"nodes": [{"commit": {"statusCheckRollup": {"contexts": {"nodes": [
            {"__typename": "CheckRun", "name": "ci", "status": "COMPLETED", "conclusion": "FAILURE",
             "checkSuite": {"workflowRun": {"workflow": {"name": "Build"}}}},
            {"__typename": "StatusContext", "context": "lint", "state": "PENDING"}
        ]}}}}]});
        let mut unchecked = node(3, "OPEN");
        unchecked["commits"] =
            serde_json::json!({"nodes": [{"commit": {"statusCheckRollup": null}}]});
        let response = serde_json::json!({"data": {
            "viewer": {"login": "me"},
            "open": {"nodes": [checked, unchecked]},
            "all": {"nodes": [node(2, "OPEN"), node(1, "MERGED"), {}]},
        }});
        let executor = Arc::new(Answer(Mutex::default(), response.to_string()));
        let service = GitHubService::new("gh", executor.clone());
        let repository = GitHubRepoKey::new("github.com", None, "owner", "repo").unwrap();
        let listing = service
            .authored_pull_requests(&repository, 50, 100, &CancellationToken::new())
            .unwrap();

        assert_eq!(listing.viewer, "me");
        assert_eq!(
            listing.open.iter().map(|pr| pr.number).collect::<Vec<_>>(),
            [2, 3]
        );
        let buckets = listing.open[0]
            .check_summaries()
            .into_iter()
            .map(|check| check.bucket.0)
            .collect::<Vec<_>>();
        assert_eq!(buckets, ["fail", "pending"]);
        assert!(matches!(
            &listing.open[0].status_check_rollup.as_deref().unwrap()[0],
            PullRequestCheckRollup::CheckRun { workflow_name, .. } if workflow_name == "Build"
        ));
        assert_eq!(
            listing.open[0].review_decision,
            Some(RemoteValue("APPROVED".into()))
        );
        assert_eq!(listing.open[0].additions, Some(7));
        assert_eq!(listing.open[1].status_check_rollup, Some(vec![]));
        assert_eq!(
            listing.all.iter().map(|pr| pr.number).collect::<Vec<_>>(),
            [2, 1]
        );
        assert_eq!(listing.all[0].status_check_rollup, None);

        let commands = executor.0.lock().unwrap();
        assert_eq!(commands.len(), 1, "both searches share one request");
        let arguments = commands[0]
            .arguments
            .iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(arguments.contains(
            &"open=repo:owner/repo is:pr author:@me sort:updated-desc is:open".to_owned()
        ));
        assert!(
            arguments
                .contains(&"all=repo:owner/repo is:pr author:@me sort:updated-desc".to_owned())
        );
        assert!(arguments.contains(&"openLimit=50".to_owned()));
        assert!(arguments.contains(&"allLimit=100".to_owned()));
    }

    #[test]
    fn a_pull_request_from_another_repository_is_refused() {
        let mut stray = node(4, "OPEN");
        stray["url"] = "https://github.com/someone/else/pull/4".into();
        let response = serde_json::json!({"data": {
            "viewer": {"login": "me"},
            "open": {"nodes": []},
            "all": {"nodes": [stray]},
        }});
        let service = GitHubService::new(
            "gh",
            Arc::new(Answer(Mutex::default(), response.to_string())),
        );
        let repository = GitHubRepoKey::new("github.com", None, "owner", "repo").unwrap();
        assert!(
            service
                .authored_pull_requests(&repository, 50, 100, &CancellationToken::new())
                .is_err()
        );
    }
}
