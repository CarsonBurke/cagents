//! The Work board's listing of the user's own pull requests: one GraphQL
//! document holding both searches and the viewer, where `gh pr list` needed a
//! process and a request for each.
use super::*;

const IDENTITY: &str = "number title state isDraft headRefName headRefOid headRepository{name} headRepositoryOwner{login} baseRefName updatedAt url";
/// A check run or status context, in the shape gh's rollup gives it.
macro_rules! context {
    () => {
        "__typename ...on CheckRun{name status conclusion detailsUrl startedAt completedAt checkSuite{workflowRun{workflow{name}}}} ...on StatusContext{context state targetUrl createdAt}"
    };
}
/// Review and check state, which GitHub computes per pull request (a hundred
/// rollups can time out a query), so it is asked for open ones alone. A
/// rollup lists a hundred checks at a time; its own state and count say what
/// all of them come to.
const REVIEW: &str = concat!(
    "reviewDecision additions deletions commits(last:1){nodes{commit{statusCheckRollup{state contexts(first:100){totalCount pageInfo{hasNextPage endCursor} nodes{",
    context!(),
    "}}}}}}"
);
/// Further pages of one pull request's checks, at the commit they belong to.
const MORE_CHECKS: &str = concat!(
    "query CagentsPullRequestChecks($owner:String!,$name:String!,$number:Int!,$cursor:String!){repository(owner:$owner,name:$name){pullRequest(number:$number){commits(last:1){nodes{commit{oid statusCheckRollup{contexts(first:100,after:$cursor){pageInfo{hasNextPage endCursor} nodes{",
    context!(),
    "}}}}}}}}}"
);
/// Pages of checks read for one pull request, the first included: a
/// thousand checks. GitHub's state of the rollup covers any beyond.
const CHECK_PAGES: usize = 10;

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
        let mut data = self.graphql(
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
        for node in data
            .pointer_mut("/open/nodes")
            .and_then(serde_json::Value::as_array_mut)
            .into_iter()
            .flatten()
        {
            self.read_more_checks(repository, node, cancellation);
        }
        Ok(AuthoredPullRequests {
            viewer,
            open: decode_authored(&data, "open", repository)?,
            all: decode_authored(&data, "all", repository)?,
        })
    }
}

impl<E: ProcessExecutor> GitHubService<E> {
    /// Appends the checks past a rollup's first page to `node`, while they
    /// belong to its head commit. A page that can't be read leaves the rest
    /// unlisted, which the rollup's own state still accounts for.
    fn read_more_checks(
        &self,
        repository: &GitHubRepoKey,
        node: &mut serde_json::Value,
        cancellation: &CancellationToken,
    ) {
        const ROLLUP: &str = "/commits/nodes/0/commit/statusCheckRollup/contexts";
        let (Some(number), Some(head)) = (
            node.get("number").and_then(serde_json::Value::as_i64),
            node.get("headRefOid")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
        ) else {
            return;
        };
        let mut page_info = node.pointer(&format!("{ROLLUP}/pageInfo")).cloned();
        for _ in 1..CHECK_PAGES {
            let Some(cursor) = page_info
                .as_ref()
                .filter(|info| info["hasNextPage"] == true)
                .and_then(|info| info["endCursor"].as_str())
                .filter(|cursor| !cursor.is_empty())
                .map(str::to_owned)
            else {
                return;
            };
            let page = match self.graphql(
                repository,
                MORE_CHECKS,
                vec![
                    ("owner", repository.owner.as_str().into()),
                    ("name", repository.name.as_str().into()),
                    ("number", GraphQlValue::Int(number)),
                    ("cursor", cursor.into()),
                ],
                cancellation,
            ) {
                Ok(page) => page,
                Err(error) => {
                    tracing::debug!(%error, number, "further checks could not be read");
                    return;
                }
            };
            let Some(commit) = page.pointer("/repository/pullRequest/commits/nodes/0/commit")
            else {
                return;
            };
            // A push since the first page: its checks are another commit's.
            if commit["oid"].as_str() != Some(head.as_str()) {
                return;
            }
            let contexts = &commit["statusCheckRollup"]["contexts"];
            let (Some(more), Some(listed)) = (
                contexts["nodes"].as_array(),
                node.pointer_mut(&format!("{ROLLUP}/nodes"))
                    .and_then(serde_json::Value::as_array_mut),
            ) else {
                return;
            };
            listed.extend(more.iter().cloned());
            page_info = Some(contexts["pageInfo"].clone());
        }
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
                let mut rollup = node
                    .pointer_mut("/commits/nodes/0/commit/statusCheckRollup")
                    .map(serde_json::Value::take)
                    .unwrap_or_default();
                let mut contexts = rollup
                    .pointer_mut("/contexts/nodes")
                    .map(serde_json::Value::take)
                    .unwrap_or_else(|| serde_json::json!([]));
                if let Some(state) = rollup.get("state").filter(|state| state.is_string()) {
                    node["statusCheckState"] = state.clone();
                }
                if let Some(count) = rollup
                    .pointer("/contexts/totalCount")
                    .filter(|count| count.is_u64())
                {
                    node["statusCheckCount"] = count.clone();
                }
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

    /// Answers each request with the next response, recording the requests.
    struct Pages(Mutex<Vec<CommandSpec>>, Mutex<Vec<serde_json::Value>>);

    impl ProcessExecutor for Pages {
        fn execute(
            &self,
            command: &CommandSpec,
            cancellation: &CancellationToken,
        ) -> Result<ProcessOutput, ProcessFailure> {
            self.0.lock().unwrap().push(command.clone());
            let response = self.1.lock().unwrap().remove(0);
            Answer(Mutex::default(), response.to_string()).execute(command, cancellation)
        }
    }

    fn checks(conclusion: &str, count: usize) -> Vec<serde_json::Value> {
        vec![
            serde_json::json!({"__typename": "CheckRun", "name": "matrix",
                "status": "COMPLETED", "conclusion": conclusion});
            count
        ]
    }

    fn more_checks(
        oid: &str,
        nodes: Vec<serde_json::Value>,
        next: Option<&str>,
    ) -> serde_json::Value {
        serde_json::json!({"data": {"repository": {"pullRequest": {"commits": {"nodes": [{"commit": {
            "oid": oid,
            "statusCheckRollup": {"contexts": {
                "pageInfo": {"hasNextPage": next.is_some(), "endCursor": next},
                "nodes": nodes,
            }},
        }}]}}}}})
    }

    fn listing_of(total: u64, state: &str) -> serde_json::Value {
        let mut long = node(5, "OPEN");
        long["commits"] = serde_json::json!({"nodes": [{"commit": {"statusCheckRollup": {
            "state": state,
            "contexts": {
                "totalCount": total,
                "pageInfo": {"hasNextPage": true, "endCursor": "page-1"},
                "nodes": checks("SUCCESS", 100),
            },
        }}}]});
        serde_json::json!({"data": {
            "viewer": {"login": "me"},
            "open": {"nodes": [long]},
            "all": {"nodes": [node(5, "OPEN")]},
        }})
    }

    fn read(responses: Vec<serde_json::Value>) -> (AuthoredPullRequests, Vec<CommandSpec>) {
        let executor = Arc::new(Pages(Mutex::default(), Mutex::new(responses)));
        let service = GitHubService::new("gh", executor.clone());
        let repository = GitHubRepoKey::new("github.com", None, "owner", "repo").unwrap();
        let listing = service
            .authored_pull_requests(&repository, 50, 100, &CancellationToken::new())
            .unwrap();
        let commands = executor.0.lock().unwrap().clone();
        (listing, commands)
    }

    #[test]
    fn checks_past_the_first_hundred_are_read_at_the_same_commit() {
        let head = "a".repeat(40);
        let mut second = checks("SUCCESS", 99);
        second.extend(checks("FAILURE", 1));
        let (listing, commands) = read(vec![
            listing_of(230, "FAILURE"),
            more_checks(&head, second, Some("page-2")),
            more_checks(&head, checks("SUCCESS", 30), None),
        ]);
        let open = &listing.open[0];
        let buckets = open
            .check_summaries()
            .into_iter()
            .map(|check| check.bucket.0)
            .collect::<Vec<_>>();
        assert_eq!(buckets.len(), 230);
        assert_eq!(buckets.iter().filter(|bucket| *bucket == "fail").count(), 1);
        assert_eq!(open.status_check_count, Some(230));
        assert_eq!(open.status_check_state, Some(RemoteValue("FAILURE".into())));
        assert_eq!(listing.all[0].status_check_count, None);
        let arguments = |command: &CommandSpec| {
            command
                .arguments
                .iter()
                .map(|argument| argument.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
        };
        assert!(arguments(&commands[0]).iter().any(|argument| {
            argument.contains("statusCheckRollup{state contexts(first:100){totalCount pageInfo{")
        }));
        for (command, cursor) in commands[1..].iter().zip(["page-1", "page-2"]) {
            let arguments = arguments(command);
            assert!(arguments.contains(&format!("query={MORE_CHECKS}")));
            assert!(arguments.contains(&format!("cursor={cursor}")));
            assert!(arguments.contains(&"number=5".to_owned()));
        }
    }

    #[test]
    fn a_push_between_pages_leaves_the_rest_to_githubs_verdict() {
        let (listing, commands) = read(vec![
            listing_of(130, "FAILURE"),
            more_checks(&"b".repeat(40), checks("FAILURE", 30), None),
        ]);
        assert_eq!(commands.len(), 2);
        let open = &listing.open[0];
        assert_eq!(open.status_check_rollup.as_ref().unwrap().len(), 100);
        assert_eq!(open.status_check_count, Some(130));
        assert_eq!(open.status_check_state, Some(RemoteValue("FAILURE".into())));
    }

    #[test]
    fn checks_stop_at_the_page_limit() {
        let head = "a".repeat(40);
        let mut responses = vec![listing_of(5_000, "PENDING")];
        responses.extend(
            (2..=CHECK_PAGES).map(|page| {
                more_checks(&head, checks("SUCCESS", 100), Some(&format!("page-{page}")))
            }),
        );
        let (listing, commands) = read(responses);
        assert_eq!(commands.len(), CHECK_PAGES);
        assert_eq!(
            listing.open[0].status_check_rollup.as_ref().unwrap().len(),
            100 * CHECK_PAGES
        );
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
