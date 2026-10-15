//! One small request detects revision/conversation changes and refreshes viewed marks.
use super::*;

#[derive(Debug, Clone)]
pub struct ReviewRefresh {
    pub context: ReviewContext,
    pub files: PullRequestFilePage,
    pub metadata: Option<PullRequest>,
}
impl<E: ProcessExecutor> GitHubService<E> {
    pub(super) fn review_refresh_snapshot(
        &self,
        key: &PullRequestKey,
        cancellation: &CancellationToken,
    ) -> Result<ReviewRefresh, GitHubError> {
        key.validate()?;
        let query = "query($owner:String!,$name:String!,$number:Int!){viewer{login} repository(owner:$owner,name:$name){pullRequest(number:$number){id number title body state isDraft headRefName headRefOid headRepository{name owner{login}} baseRefName baseRefOid author{login} updatedAt url reviewDecision viewerDidAuthor reviews(first:10,states:[PENDING]){nodes{id author{login} commit{oid} comments{totalCount}} pageInfo{hasNextPage endCursor}} files(first:100){nodes{path additions deletions changeType viewerViewedState} pageInfo{hasNextPage endCursor}}}}}";
        let data = self.graphql(
            &key.repository,
            query,
            Self::pull_request_variables(key),
            cancellation,
        )?;
        let pr = data.pointer("/repository/pullRequest").ok_or_else(|| {
            GitHubError::Inspection("GitHub returned no pull request refresh".into())
        })?;
        let viewer: GitHubActor = decode_value(&data["viewer"])?;
        let page: ReviewContextPage = decode_value(pr)?;
        let pending = page
            .reviews
            .nodes
            .into_iter()
            .find(|review| {
                review
                    .author
                    .as_ref()
                    .is_some_and(|author| author.login == viewer.login)
            })
            .map(|review| PendingReview {
                id: review.id,
                comment_count: review.comments.total_count,
                commit_oid: review.commit.map(|commit| commit.oid),
            });
        let context = ReviewContext {
            pull_request_id: page.id,
            head_oid: page.head_ref_oid,
            base_oid: page.base_ref_oid,
            viewer: viewer.login,
            viewer_is_author: page.viewer_did_author,
            pending,
        };
        let connection: Connection<PullRequestFile> = decode_value(&pr["files"])?;
        let cursor = connection
            .page_info
            .has_next_page
            .then(|| next_cursor(&connection.page_info, None))
            .transpose()?;
        let mut metadata=pr.clone();
        metadata["headRepositoryOwner"]=pr.pointer("/headRepository/owner").cloned().unwrap_or(serde_json::Value::Null);
        let metadata: PullRequest = decode_value(&metadata)?;
        validate_pull_request(&metadata, &key.repository)?;
        Ok(ReviewRefresh {
            files: PullRequestFilePage {
                files: connection.nodes,
                next_cursor: cursor,
                pull_request_id: context.pull_request_id.clone(),
                head_oid: context.head_oid.clone(),
                base_oid: context.base_oid.clone(),
                viewer: context.viewer.clone(),
            },
            context,
            metadata: Some(metadata),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct OneCall {
        calls: std::sync::atomic::AtomicUsize,
    }
    impl ProcessExecutor for OneCall {
        fn execute(
            &self,
            command: &CommandSpec,
            _: &CancellationToken,
        ) -> Result<ProcessOutput, ProcessFailure> {
            self.calls
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            assert!(command.arguments.iter().any(|arg| arg == "--include"));
            let body = serde_json::json!({"data":{"viewer":{"login":"reviewer"},"repository":{"pullRequest":{
                "id":"PR_1","number":1,"title":"Review","state":"OPEN","headRefName":"feature","headRefOid":"a".repeat(40),"headRepository":{"name":"fork","owner":{"login":"author"}},"baseRefName":"main","baseRefOid":"b".repeat(40),"updatedAt":"2026-10-14T00:00:00Z","url":"https://refresh-test.invalid/o/r/pull/1","viewerDidAuthor":false,
                "reviews":{"nodes":[],"pageInfo":{"hasNextPage":false,"endCursor":null}},
                "files":{"nodes":[{"path":"file.rs","viewerViewedState":"VIEWED"}],"pageInfo":{"hasNextPage":false,"endCursor":null}}
            }}}});
            Ok(ProcessOutput {
                success: true,
                exit_code: Some(0),
                signal: None,
                stdout: format!("HTTP/2.0 200 OK\r\nX-RateLimit-Remaining: 4000\r\n\r\n{body}")
                    .into_bytes(),
                stderr: vec![],
            })
        }
    }
    #[test]
    fn refresh_context_viewed_marks_and_freshness_share_one_graphql_request() {
        let executor = Arc::new(OneCall {
            calls: Default::default(),
        });
        let service = GitHubService::new("gh", executor.clone());
        let key = PullRequestKey::new(
            GitHubRepoKey::new("refresh-test.invalid", None, "o", "r").unwrap(),
            1,
        )
        .unwrap();
        let refresh = service
            .review_refresh_snapshot(&key, &CancellationToken::new())
            .unwrap();
        assert_eq!(executor.calls.load(std::sync::atomic::Ordering::Relaxed), 1);
        assert_eq!(refresh.context.viewer, "reviewer");
        assert_eq!(refresh.metadata.as_ref().unwrap().head_repository_key(),Some(GitHubRepoKey::new("refresh-test.invalid",None,"author","fork").unwrap()));
        assert_eq!(
            refresh.files.files[0].viewer_viewed_state,
            FileViewedState::Viewed
        );
        assert_eq!(refresh.metadata.unwrap().updated_at, "2026-10-14T00:00:00Z");
    }
}
