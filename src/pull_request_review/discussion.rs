//! Immutable review operations and their last-moment authority checks.
//!
//! UI generations fence responses; this layer also rechecks the repository,
//! viewer and exact diff head before any write crosses the GitHub boundary.

use super::*;
use crate::github::{
    AddReviewThreadRequest, PendingReview, PullRequestConversation, ReplyToThreadRequest,
    ReviewDiffSide, ReviewEvent, ReviewThreadSubject, SubmitReviewRequest,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Anchor {
    pub path: String,
    pub side: ReviewDiffSide,
    pub start_line: u64,
    pub line: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mutation {
    Comment {
        anchor: Anchor,
        body: String,
    },
    /// A thread on the file as a whole rather than any line of its diff.
    FileComment {
        path: String,
        body: String,
    },
    Reply {
        thread: String,
        body: String,
    },
    Edit {
        comment: String,
        body: String,
    },
    Delete {
        comment: String,
    },
    Resolve {
        thread: String,
        resolved: bool,
    },
    Submit {
        request: SubmitReviewRequest,
        comments: Vec<PendingComment>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingComment {
    id: String,
    body: String,
    path: String,
    side: ReviewDiffSide,
    line: Option<u64>,
    start_line: Option<u64>,
}

pub fn pending_snapshot(
    conversation: &PullRequestConversation,
    pending: Option<&PendingReview>,
) -> Result<Vec<PendingComment>, Outcome> {
    let expected = pending.map_or(0, |review| review.comment_count);
    if expected == 0 {
        return Ok(Vec::new());
    }
    if conversation.truncated || conversation.threads.iter().any(|thread| thread.truncated) {
        return Err(Outcome::Refused(
            "The pending discussion is incomplete. Review and submit it on GitHub.".into(),
        ));
    }
    let mut comments = Vec::new();
    for thread in &conversation.threads {
        for comment in &thread.comments {
            if !comment.viewer_did_author || !comment.is_pending() {
                continue;
            }
            let id = comment.id.clone().ok_or_else(|| {
                Outcome::Refused(
                    "A pending comment has no verified identity. Refresh before submitting.".into(),
                )
            })?;
            comments.push(PendingComment {
                id,
                body: comment.body.clone(),
                path: thread.path.clone(),
                side: thread.side,
                line: thread.line,
                start_line: thread.start_line,
            });
        }
    }
    comments.sort_by(|a, b| a.id.cmp(&b.id));
    if comments.len() as u64 != expected || comments.windows(2).any(|pair| pair[0].id == pair[1].id)
    {
        return Err(Outcome::Refused(
            "The pending comment list changed or is incomplete. Refresh before submitting.".into(),
        ));
    }
    Ok(comments)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Authority {
    pub target: ReviewTarget,
    pub context: ReviewContext,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Applied,
    Refused(String),
    /// A write may have crossed the process boundary. Never automatically
    /// resend it; the UI must reconcile the pending review first.
    Unknown(String),
}

fn outcome<T>(
    result: Result<ReviewMutationOutcome<T>, crate::github::GitHubError>,
) -> Result<T, Outcome> {
    match result {
        Ok(ReviewMutationOutcome::Applied(value)) => Ok(value),
        Ok(ReviewMutationOutcome::NotApplied { failure }) => {
            Err(Outcome::Refused(failure.to_string()))
        }
        Ok(ReviewMutationOutcome::OutcomeUnknown { failure }) => {
            Err(Outcome::Unknown(failure.to_string()))
        }
        Err(error) => Err(Outcome::Refused(error.to_string())),
    }
}

fn same_authority(expected: &ReviewContext, current: &ReviewContext) -> Result<(), Outcome> {
    if expected.pull_request_id != current.pull_request_id
        || expected.head_oid != current.head_oid
        || expected.base_oid != current.base_oid
        || expected.viewer != current.viewer
    {
        return Err(Outcome::Refused(
            "The pull request, diff head, base, or signed-in account changed. Refresh before reviewing.".into(),
        ));
    }
    Ok(())
}

fn pending_review(
    api: &dyn GitHubReviewApi,
    repository: &GitHubRepoKey,
    context: &ReviewContext,
    cancellation: &CancellationToken,
) -> Result<PendingReview, Outcome> {
    let pending = match &context.pending {
        Some(pending) => pending.clone(),
        None => outcome(api.start_review(
            repository,
            &context.pull_request_id,
            &context.head_oid,
            cancellation,
        ))?,
    };
    if pending.commit_oid.as_deref() != Some(context.head_oid.as_str()) {
        return Err(Outcome::Refused(
            "Your pending review belongs to another commit. Review or discard that draft on GitHub before adding comments to this head.".into(),
        ));
    }
    Ok(pending)
}

/// Called only on the blocking pool. The captured authority is also the text
/// rendered in the submission confirmation, rather than a mutable selection.
pub fn execute(
    api: &dyn GitHubReviewApi,
    git: &RepositoryService,
    authority: &Authority,
    mutation: &Mutation,
    cancellation: &CancellationToken,
) -> Outcome {
    let run = || -> Result<(), Outcome> {
        let (key, current) = verify_authority(api, git, authority, cancellation)?;
        let target = &authority.target;
        execute_current(
            api,
            &target.repository,
            &key,
            &current,
            &authority.context,
            mutation,
            cancellation,
        )
    };
    match run() {
        Ok(()) => Outcome::Applied,
        Err(result) => result,
    }
}

pub(super) fn verify_authority(
    api: &dyn GitHubReviewApi,
    git: &RepositoryService,
    authority: &Authority,
    cancellation: &CancellationToken,
) -> Result<(PullRequestKey, ReviewContext), Outcome> {
    if cancellation.is_cancelled() {
        return Err(Outcome::Refused(
            "The review was closed before sending.".into(),
        ));
    }
    let target = &authority.target;
    let remotes = git
        .remotes(&target.worktree, cancellation)
        .map_err(|error| Outcome::Refused(error.to_string()))?;
    if remote_for_repository(&remotes, &target.repository).is_none() {
        return Err(Outcome::Refused(
            "This worktree no longer points at the reviewed GitHub repository.".into(),
        ));
    }
    let key = PullRequestKey::new(target.repository.clone(), target.pull_request.number)
        .map_err(|error| Outcome::Refused(error.to_string()))?;
    let current = api
        .review_context(&key, cancellation)
        .map_err(|error| Outcome::Refused(error.to_string()))?;
    same_authority(&authority.context, &current)?;
    Ok((key, current))
}

fn execute_current(
    api: &dyn GitHubReviewApi,
    repository: &GitHubRepoKey,
    key: &PullRequestKey,
    current: &ReviewContext,
    expected: &ReviewContext,
    mutation: &Mutation,
    cancellation: &CancellationToken,
) -> Result<(), Outcome> {
    if cancellation.is_cancelled() {
        return Err(Outcome::Refused(
            "The review was closed before sending.".into(),
        ));
    }
    if let Mutation::Submit { request, comments } = mutation {
        if request.pull_request_id != current.pull_request_id
            || request.commit_oid != current.head_oid
            || request.review_id.as_deref()
                != current.pending.as_ref().map(|review| review.id.as_str())
            || expected.pending != current.pending
        {
            return Err(Outcome::Refused(
                "Your pending review changed. Refresh and confirm its comments before submitting."
                    .into(),
            ));
        }
        if current.viewer_is_author && request.event != ReviewEvent::Comment {
            return Err(Outcome::Refused("You can comment on your own pull request, but cannot approve it or request changes.".into()));
        }
        if current
            .pending
            .as_ref()
            .is_some_and(|pending| pending.commit_oid.as_deref() != Some(current.head_oid.as_str()))
        {
            return Err(Outcome::Refused("The pending review belongs to another commit. Refresh its review on GitHub before submitting.".into()));
        }
        if current
            .pending
            .as_ref()
            .is_some_and(|pending| pending.comment_count > 0)
        {
            let conversation = api
                .conversation(key, cancellation)
                .map_err(|error| Outcome::Refused(error.to_string()))?;
            if pending_snapshot(&conversation, current.pending.as_ref())? != *comments {
                return Err(Outcome::Refused("Your pending comments changed after confirmation. Refresh and review them before submitting.".into()));
            }
        } else if !comments.is_empty() {
            return Err(Outcome::Refused(
                "Your pending comments changed after confirmation.".into(),
            ));
        }
        return outcome(api.submit_review(repository, request, cancellation)).map(drop);
    }
    let thread = match mutation {
        Mutation::Comment { anchor, body } => Some(AddReviewThreadRequest {
            review_id: "pending".into(),
            path: anchor.path.clone(),
            subject: ReviewThreadSubject::Lines {
                line: anchor.line,
                side: anchor.side,
                start_line: (anchor.start_line != anchor.line).then_some(anchor.start_line),
                start_side: (anchor.start_line != anchor.line).then_some(anchor.side),
            },
            body: body.clone(),
        }),
        Mutation::FileComment { path, body } => Some(AddReviewThreadRequest {
            review_id: "pending".into(),
            path: path.clone(),
            subject: ReviewThreadSubject::File,
            body: body.clone(),
        }),
        _ => None,
    };
    if let Some(mut request) = thread {
        // Validate before creating a pending review, so a blank composer or
        // invalid range never leaves an empty remote draft behind.
        request
            .validate()
            .map_err(|error| Outcome::Refused(error.to_string()))?;
        request.review_id = pending_review(api, repository, current, cancellation)?.id;
        return outcome(api.add_review_thread(repository, &request, cancellation)).map(drop);
    }
    let conversation = api
        .conversation(key, cancellation)
        .map_err(|error| Outcome::Refused(error.to_string()))?;
    match mutation {
        Mutation::Reply { thread, body } => {
            if !conversation
                .threads
                .iter()
                .any(|candidate| candidate.id == *thread)
            {
                return Err(Outcome::Refused(
                    "That review thread is no longer available. Refresh before replying.".into(),
                ));
            }
            let mut request = ReplyToThreadRequest {
                thread_id: thread.clone(),
                review_id: None,
                body: body.clone(),
            };
            request
                .validate()
                .map_err(|error| Outcome::Refused(error.to_string()))?;
            request.review_id = Some(pending_review(api, repository, current, cancellation)?.id);
            outcome(api.reply_to_thread(repository, &request, cancellation)).map(drop)
        }
        Mutation::Edit { comment, body } => {
            let candidate = own_pending_comment(&conversation, comment)?;
            if !candidate.viewer_can_update {
                return Err(Outcome::Refused(
                    "GitHub no longer permits editing that comment.".into(),
                ));
            }
            outcome(api.update_comment(repository, comment, body, cancellation)).map(drop)
        }
        Mutation::Delete { comment } => {
            let candidate = own_pending_comment(&conversation, comment)?;
            if !candidate.viewer_can_delete {
                return Err(Outcome::Refused(
                    "GitHub no longer permits deleting that comment.".into(),
                ));
            }
            outcome(api.delete_comment(repository, comment, cancellation))
        }
        Mutation::Resolve { thread, resolved } => {
            let candidate = conversation
                .threads
                .iter()
                .find(|candidate| candidate.id == *thread)
                .ok_or_else(|| {
                    Outcome::Refused("That review thread is no longer available.".into())
                })?;
            if (*resolved && !candidate.viewer_can_resolve)
                || (!*resolved && !candidate.viewer_can_unresolve)
            {
                return Err(Outcome::Refused(
                    "GitHub no longer permits changing this thread's resolution.".into(),
                ));
            }
            outcome(api.set_thread_resolved(repository, thread, *resolved, cancellation))
        }
        Mutation::Comment { .. } | Mutation::FileComment { .. } | Mutation::Submit { .. } => {
            unreachable!()
        }
    }
}

fn own_pending_comment<'a>(
    conversation: &'a PullRequestConversation,
    id: &str,
) -> Result<&'a crate::github::PullRequestComment, Outcome> {
    conversation
        .threads
        .iter()
        .flat_map(|thread| &thread.comments)
        .find(|comment| {
            comment.id.as_deref() == Some(id) && comment.viewer_did_author && comment.is_pending()
        })
        .ok_or_else(|| {
            Outcome::Refused(
                "Only your own pending comments can be changed here. Refresh before trying again."
                    .into(),
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn context() -> ReviewContext {
        ReviewContext {
            pull_request_id: "PR_1".into(),
            head_oid: "a".repeat(40),
            base_oid: "b".repeat(40),
            viewer: "reviewer".into(),
            viewer_is_author: false,
            pending: Some(PendingReview {
                id: "REVIEW_1".into(),
                comment_count: 2,
                commit_oid: Some("a".repeat(40)),
            }),
        }
    }
    fn repository() -> GitHubRepoKey {
        GitHubRepoKey {
            host: "github.com".into(),
            port: None,
            owner: "owner".into(),
            name: "repo".into(),
        }
    }

    #[test]
    fn changed_head_base_viewer_or_pull_request_refuses_captured_authority() {
        let expected = context();
        assert_eq!(same_authority(&expected, &expected), Ok(()));
        for changed in 0..4 {
            let mut current = expected.clone();
            match changed {
                0 => current.head_oid = "c".repeat(40),
                1 => current.base_oid = "c".repeat(40),
                2 => current.viewer = "different-account".into(),
                _ => current.pull_request_id = "PR_other".into(),
            }
            assert!(matches!(
                same_authority(&expected, &current),
                Err(Outcome::Refused(_))
            ));
        }
    }

    #[test]
    fn adopts_existing_pending_review_only_at_the_rendered_commit() {
        let api = ScriptedApi::new(); // Every review write panics: adoption must perform none.
        let mut current = context();
        let cancellation = CancellationToken::new();
        assert_eq!(
            pending_review(&api, &repository(), &current, &cancellation)
                .unwrap()
                .id,
            "REVIEW_1"
        );
        current.pending.as_mut().unwrap().commit_oid = Some("c".repeat(40));
        assert!(matches!(
            pending_review(&api, &repository(), &current, &cancellation),
            Err(Outcome::Refused(_))
        ));
        current.pending.as_mut().unwrap().commit_oid = None;
        assert!(matches!(
            pending_review(&api, &repository(), &current, &cancellation),
            Err(Outcome::Refused(_))
        ));
    }

    #[test]
    fn changed_pending_review_cannot_silently_add_comments_to_confirmed_submission() {
        let api = ScriptedApi::new();
        let expected = context();
        let mut current = expected.clone();
        current.pending.as_mut().unwrap().comment_count += 1;
        let repo = repository();
        let request = SubmitReviewRequest {
            pull_request_id: expected.pull_request_id.clone(),
            review_id: Some("REVIEW_1".into()),
            commit_oid: expected.head_oid.clone(),
            event: ReviewEvent::Approve,
            body: "Reviewed".into(),
        };
        assert!(matches!(
            execute_current(
                &api,
                &repo,
                &PullRequestKey::new(repo.clone(), 1).unwrap(),
                &current,
                &expected,
                &Mutation::Submit {
                    request,
                    comments: Vec::new()
                },
                &CancellationToken::new()
            ),
            Err(Outcome::Refused(_))
        ));
    }

    #[test]
    fn same_count_edit_or_incomplete_evidence_cannot_change_confirmed_comments() {
        let api = ScriptedApi::new();
        let mut current = context();
        current.pending.as_mut().unwrap().comment_count = 1;
        let comment = serde_json::from_value(serde_json::json!({
            "id":"comment-1", "author":{"login":"reviewer"}, "body":"Confirmed text", "createdAt":"2026-09-01T00:00:00Z",
            "url":"https://github.com/owner/repo/pull/1#comment-1", "viewerDidAuthor":true, "commentState":"PENDING"
        })).unwrap();
        let mut conversation = PullRequestConversation {
            threads: vec![crate::github::PullRequestReviewThread {
                id: "thread-1".into(),
                path: "file.rs".into(),
                line: Some(7),
                start_line: Some(4),
                side: ReviewDiffSide::Right,
                start_side: Some(ReviewDiffSide::Right),
                outdated: false,
                resolved: false,
                collapsed: false,
                viewer_can_resolve: true,
                viewer_can_unresolve: false,
                comments: vec![comment],
                truncated: false,
            }],
            ..Default::default()
        };
        let comments = pending_snapshot(&conversation, current.pending.as_ref()).unwrap();
        let request = SubmitReviewRequest {
            pull_request_id: current.pull_request_id.clone(),
            review_id: Some("REVIEW_1".into()),
            commit_oid: current.head_oid.clone(),
            event: ReviewEvent::Approve,
            body: "Summary".into(),
        };
        let mutation = Mutation::Submit { request, comments };
        let repo = repository();
        let key = PullRequestKey::new(repo.clone(), 1).unwrap();
        let cancellation = CancellationToken::new();
        conversation.threads[0].comments[0].body = "Changed in another client".into();
        *api.conversation.lock().unwrap() = conversation.clone();
        assert!(matches!(
            execute_current(
                &api,
                &repo,
                &key,
                &current,
                &current,
                &mutation,
                &cancellation
            ),
            Err(Outcome::Refused(_))
        ));
        assert!(api.submitted.lock().unwrap().is_empty());
        conversation.threads[0].comments[0].body = "Confirmed text".into();
        conversation.truncated = true;
        *api.conversation.lock().unwrap() = conversation.clone();
        assert!(matches!(
            execute_current(
                &api,
                &repo,
                &key,
                &current,
                &current,
                &mutation,
                &cancellation
            ),
            Err(Outcome::Refused(_))
        ));
        assert!(api.submitted.lock().unwrap().is_empty());
        conversation.truncated = false;
        *api.conversation.lock().unwrap() = conversation;
        assert_eq!(
            execute_current(
                &api,
                &repo,
                &key,
                &current,
                &current,
                &mutation,
                &cancellation
            ),
            Ok(())
        );
        assert_eq!(api.submitted.lock().unwrap().len(), 1);
    }

    #[test]
    fn blank_line_comment_does_not_create_a_remote_pending_review() {
        let api = ScriptedApi::new();
        let mut current = context();
        current.pending = None;
        let repo = repository();
        let mutation = Mutation::Comment {
            anchor: Anchor {
                path: "file.rs".into(),
                side: ReviewDiffSide::Right,
                start_line: 1,
                line: 1,
            },
            body: "  \n".into(),
        };
        assert!(matches!(
            execute_current(
                &api,
                &repo,
                &PullRequestKey::new(repo.clone(), 1).unwrap(),
                &current,
                &current,
                &mutation,
                &CancellationToken::new()
            ),
            Err(Outcome::Refused(_))
        ));
    }

    #[test]
    fn blank_or_escaping_file_comment_does_not_create_a_remote_pending_review() {
        let api = ScriptedApi::new();
        let mut current = context();
        current.pending = None;
        let repo = repository();
        for (path, body) in [("file.rs", " \n"), ("../outside.rs", "Looks good")] {
            let mutation = Mutation::FileComment {
                path: path.into(),
                body: body.into(),
            };
            assert!(matches!(
                execute_current(
                    &api,
                    &repo,
                    &PullRequestKey::new(repo.clone(), 1).unwrap(),
                    &current,
                    &current,
                    &mutation,
                    &CancellationToken::new()
                ),
                Err(Outcome::Refused(_))
            ));
        }
    }
}
