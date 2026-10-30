//! Immutable review operations and their last-moment authority checks.
//!
//! UI generations fence responses; this layer also rechecks the repository,
//! viewer and exact diff head before any write crosses the GitHub boundary.

use super::*;
use crate::github::{
    AddReviewThreadRequest, PendingReview, PullRequestConversation, ReplyToThreadRequest,
    ReviewDiffSide, ReviewEvent, ReviewThreadSubject, SubmitReviewRequest,
};

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
        single: bool,
    },
    /// A thread on the file as a whole rather than any line of its diff.
    FileComment {
        path: String,
        body: String,
        single: bool,
    },
    Reply {
        thread: String,
        body: String,
        add_to_review: bool,
    },
    Discard {
        review_id: String,
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
    // The exact pending-review count and unique IDs below prove whether its
    // comments are complete, independent of unrelated conversation page caps.
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
    pub merge_base: Option<Oid>,
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

pub(super) fn same_identity(
    expected: &ReviewContext,
    current: &ReviewContext,
) -> Result<(), Outcome> {
    if expected.pull_request_id != current.pull_request_id || expected.viewer != current.viewer {
        return Err(Outcome::Refused(
            "The pull request or signed-in account changed. Refresh before reviewing.".into(),
        ));
    }
    Ok(())
}
fn same_authority(expected: &ReviewContext, current: &ReviewContext) -> Result<(), Outcome> {
    same_identity(expected, current)?;
    if expected.head_oid != current.head_oid || expected.base_oid != current.base_oid {
        return Err(Outcome::Refused(
            "The diff revision changed. Refresh before reviewing.".into(),
        ));
    }
    Ok(())
}
fn mutation_authority(
    expected: &ReviewContext,
    current: &ReviewContext,
    mutation: &Mutation,
) -> Result<(), Outcome> {
    same_identity(expected, current)?;
    match mutation {
        Mutation::Submit { .. } => same_authority(expected, current),
        Mutation::Comment { .. } | Mutation::FileComment { .. } if expected.head_oid != current.head_oid => {
            Err(Outcome::Refused("The pull request head changed. Your draft is kept; re-anchor it to the current head before posting.".into()))
        }
        _ => Ok(()),
    }
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
            "Your pending review belongs to another commit. Use Discard pending review before adding comments to this head.".into(),
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
        let (key, current) = verify_identity(api, git, authority, cancellation)?;
        mutation_authority(&authority.context, &current, mutation)?;
        verify_anchor_base(api, &key, authority, &current, mutation, cancellation)?;
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

fn verify_anchor_base(
    api: &dyn GitHubReviewApi,
    key: &PullRequestKey,
    authority: &Authority,
    current: &ReviewContext,
    mutation: &Mutation,
    cancellation: &CancellationToken,
) -> Result<(), Outcome> {
    if matches!(
        mutation,
        Mutation::Comment {
            anchor: Anchor {
                side: ReviewDiffSide::Left,
                ..
            },
            ..
        }
    ) && authority.context.base_oid != current.base_oid
    {
        let merge_base = api
            .review_merge_base(key, &current.base_oid, &current.head_oid, cancellation)
            .map_err(|error| Outcome::Refused(error.to_string()))?;
        if authority
            .merge_base
            .as_ref()
            .is_none_or(|expected| expected.as_str() != merge_base)
        {
            return Err(Outcome::Refused("The diff's merge base changed. Your draft is kept; refresh and re-anchor it before posting.".into()));
        }
    }
    Ok(())
}

pub(super) fn verify_authority(
    api: &dyn GitHubReviewApi,
    git: &RepositoryService,
    authority: &Authority,
    cancellation: &CancellationToken,
) -> Result<(PullRequestKey, ReviewContext), Outcome> {
    let (key, current) = verify_identity(api, git, authority, cancellation)?;
    same_authority(&authority.context, &current)?;
    Ok((key, current))
}

fn verify_identity(
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
    same_identity(&authority.context, &current)?;
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
    mutation_authority(expected, current, mutation)?;
    if let Mutation::Discard { review_id } = mutation {
        if current.pending.as_ref().map(|p| &p.id) != Some(review_id)
            || expected.pending != current.pending
        {
            return Err(Outcome::Refused(
                "The pending review changed. Refresh before discarding it.".into(),
            ));
        }
        return outcome(api.discard_review(repository, review_id, cancellation));
    }
    if let Mutation::Submit { request, comments } = mutation {
        if request.event == ReviewEvent::Comment
            && request.body.trim().is_empty()
            && comments.is_empty()
        {
            return Err(Outcome::Refused("Write a review summary or add a pending comment before submitting a Comment review.".into()));
        }
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
            return Err(Outcome::Refused("The pending review belongs to another commit. Use Discard pending review, then start a review on the current head.".into()));
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
        Mutation::Comment { anchor, body, .. } => Some(AddReviewThreadRequest {
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
        Mutation::FileComment { path, body, .. } => Some(AddReviewThreadRequest {
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
        let single = matches!(
            mutation,
            Mutation::Comment { single: true, .. } | Mutation::FileComment { single: true, .. }
        );
        if single {
            return outcome(api.add_single_review_thread(
                key,
                &current.head_oid,
                &request,
                cancellation,
            ))
            .map(drop);
        }
        request.review_id = pending_review(api, repository, current, cancellation)?.id;
        return outcome(api.add_review_thread(repository, &request, cancellation)).map(drop);
    }
    let conversation = api
        .conversation(key, cancellation)
        .map_err(|error| Outcome::Refused(error.to_string()))?;
    match mutation {
        Mutation::Reply {
            thread,
            body,
            add_to_review,
        } => {
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
            if *add_to_review {
                request.review_id =
                    Some(pending_review(api, repository, current, cancellation)?.id);
            }
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
        Mutation::Comment { .. }
        | Mutation::FileComment { .. }
        | Mutation::Submit { .. }
        | Mutation::Discard { .. } => {
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
    fn left_anchor_accepts_unrelated_base_move_but_refuses_a_new_merge_base() {
        let api = ScriptedApi::new();
        let expected = context();
        let mut current = expected.clone();
        current.base_oid = "c".repeat(40);
        let base = "b".repeat(40);
        *api.merge_base.lock().unwrap() = Some(base.clone());
        let target = super::super::tests::target(1);
        let key = PullRequestKey::new(target.repository.clone(), 1).unwrap();
        let authority = Authority {
            target,
            context: expected,
            merge_base: Some(Oid::parse(&base).unwrap()),
        };
        let mutation = Mutation::Comment {
            anchor: Anchor {
                path: "file.rs".into(),
                side: ReviewDiffSide::Left,
                start_line: 1,
                line: 1,
            },
            body: "Anchored".into(),
            single: false,
        };
        assert_eq!(
            verify_anchor_base(
                &api,
                &key,
                &authority,
                &current,
                &mutation,
                &CancellationToken::new()
            ),
            Ok(())
        );
        *api.merge_base.lock().unwrap() = Some("d".repeat(40));
        assert!(matches!(
            verify_anchor_base(
                &api,
                &key,
                &authority,
                &current,
                &mutation,
                &CancellationToken::new()
            ),
            Err(Outcome::Refused(_))
        ));
    }

    #[test]
    fn mutation_authority_uses_identity_for_existing_comments_and_head_for_new_anchors() {
        let expected = context();
        let mutations = [
            Mutation::Reply {
                thread: "thread".into(),
                body: "reply".into(),
                add_to_review: false,
            },
            Mutation::Resolve {
                thread: "thread".into(),
                resolved: true,
            },
            Mutation::Edit {
                comment: "comment".into(),
                body: "edit".into(),
            },
            Mutation::Delete {
                comment: "comment".into(),
            },
        ];
        let line = Mutation::Comment {
            anchor: Anchor {
                path: "file.rs".into(),
                side: ReviewDiffSide::Right,
                start_line: 1,
                line: 1,
            },
            body: "comment".into(),
            single: false,
        };
        for changed in ["base", "head", "viewer", "pr"] {
            let mut current = expected.clone();
            match changed {
                "base" => current.base_oid = "c".repeat(40),
                "head" => current.head_oid = "c".repeat(40),
                "viewer" => current.viewer = "other".into(),
                _ => current.pull_request_id = "PR_other".into(),
            }
            for mutation in &mutations {
                assert_eq!(
                    mutation_authority(&expected, &current, mutation).is_ok(),
                    matches!(changed, "base" | "head"),
                    "{changed}: {mutation:?}"
                );
            }
            assert_eq!(
                mutation_authority(&expected, &current, &line).is_ok(),
                changed == "base"
            );
        }
    }
    #[test]
    fn immediate_reply_sends_no_review_id_after_head_and_base_move() {
        let api = ScriptedApi::new();
        let expected = context();
        let mut current = expected.clone();
        current.base_oid = "c".repeat(40);
        current.head_oid = "d".repeat(40);
        api.conversation
            .lock()
            .unwrap()
            .threads
            .push(crate::github::PullRequestReviewThread {
                id: "thread".into(),
                path: "file.rs".into(),
                line: Some(1),
                start_line: None,
                side: ReviewDiffSide::Right,
                start_side: None,
                outdated: true,
                resolved: false,
                collapsed: false,
                viewer_can_resolve: true,
                viewer_can_unresolve: false,
                comments: vec![],
                truncated: false,
            });
        let repo = repository();
        assert_eq!(
            execute_current(
                &api,
                &repo,
                &PullRequestKey::new(repo.clone(), 1).unwrap(),
                &current,
                &expected,
                &Mutation::Reply {
                    thread: "thread".into(),
                    body: "Visible immediately".into(),
                    add_to_review: false
                },
                &CancellationToken::new()
            ),
            Ok(())
        );
        let replies = api.replies.lock().unwrap();
        assert_eq!(replies.len(), 1);
        assert!(replies[0].review_id.is_none());
    }
    #[test]
    fn stale_review_can_be_discarded_without_current_head_match() {
        let api = ScriptedApi::new();
        let mut current = context();
        current.head_oid = "c".repeat(40);
        let repo = repository();
        assert_eq!(
            execute_current(
                &api,
                &repo,
                &PullRequestKey::new(repo.clone(), 1).unwrap(),
                &current,
                &current,
                &Mutation::Discard {
                    review_id: "REVIEW_1".into()
                },
                &CancellationToken::new()
            ),
            Ok(())
        );
        assert_eq!(&*api.discarded.lock().unwrap(), &["REVIEW_1"]);
    }

    #[test]
    fn discard_refuses_changed_pending_scope_but_allows_an_unchanged_review_after_diff_moves() {
        let mut expected = context();
        expected.pending.as_mut().unwrap().comment_count = 1;
        let repo = repository();
        let key = PullRequestKey::new(repo.clone(), 1).unwrap();
        let mutation = Mutation::Discard {
            review_id: "REVIEW_1".into(),
        };
        for changed in [
            "added comment",
            "removed comment",
            "commit",
            "identity",
            "absent",
        ] {
            let api = ScriptedApi::new();
            let mut current = expected.clone();
            match changed {
                "added comment" => current.pending.as_mut().unwrap().comment_count = 2,
                "removed comment" => current.pending.as_mut().unwrap().comment_count = 0,
                "commit" => current.pending.as_mut().unwrap().commit_oid = Some("c".repeat(40)),
                "identity" => current.pending.as_mut().unwrap().id = "REVIEW_other".into(),
                "absent" => current.pending = None,
                _ => unreachable!(),
            }
            assert!(
                matches!(
                    execute_current(
                        &api,
                        &repo,
                        &key,
                        &current,
                        &expected,
                        &mutation,
                        &CancellationToken::new(),
                    ),
                    Err(Outcome::Refused(_))
                ),
                "{changed} must require a new discard confirmation"
            );
            assert!(api.discarded.lock().unwrap().is_empty(), "{changed}");
        }
        let api = ScriptedApi::new();
        let mut current = expected.clone();
        current.head_oid = "c".repeat(40);
        current.base_oid = "d".repeat(40);
        assert_eq!(
            execute_current(
                &api,
                &repo,
                &key,
                &current,
                &expected,
                &mutation,
                &CancellationToken::new(),
            ),
            Ok(()),
            "unchanged pending work can be discarded even after its diff moves"
        );
        assert_eq!(&*api.discarded.lock().unwrap(), &["REVIEW_1"]);
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
        conversation.threads_truncated = true;
        let saved_comment = conversation.threads[0].comments.pop().unwrap();
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
        conversation.threads[0].comments.push(saved_comment);
        // Unrelated threads and issue comments may truncate while every
        // confirmed pending comment is still present and uniquely identified.
        conversation.threads_truncated = true;
        conversation.truncated = true; // Only issue comments/reviews hit their page cap.
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
            single: false,
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
                single: false,
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

/// Map an anchored range only when its surrounding text is unique and unchanged.
/// Ambiguous or changed content stays a draft until the reader chooses a line.
fn map_range(old: &str, new: &str, start: u64, end: u64) -> Result<(u64, u64), String> {
    let old_lines: Vec<_> = old.lines().collect();
    let new_lines: Vec<_> = new.lines().collect();
    let first = usize::try_from(start.saturating_sub(1)).map_err(|_| "Invalid draft range")?;
    let last = usize::try_from(end).map_err(|_| "Invalid draft range")?;
    if start == 0 || first >= last || last > old_lines.len() {
        return Err("The original anchor is unavailable. Pick its new line in the gutter; your draft text is kept.".into());
    }
    let from = first.saturating_sub(3);
    let to = last.saturating_add(3).min(old_lines.len());
    let context = &old_lines[from..to];
    let matches: Vec<_> = new_lines
        .windows(context.len())
        .enumerate()
        .filter(|(_, lines)| *lines == context)
        .map(|(i, _)| i)
        .take(2)
        .collect();
    if matches.len() != 1 {
        return Err("The anchor changed or is ambiguous. Pick its new line in the gutter; your draft text is kept.".into());
    }
    let mapped = matches[0] + first - from + 1;
    Ok((mapped as u64, (mapped + last - first - 1) as u64))
}

pub(super) fn reanchor(
    git: &RepositoryService,
    worktree: &Path,
    old_range: &PullRequestRange,
    new_range: &PullRequestRange,
    anchor: &Anchor,
    cancellation: &CancellationToken,
) -> Result<Anchor, String> {
    let read = |range: &PullRequestRange| {
        let diff = git
            .diff(
                &crate::repository::DiffRequest {
                    worktree: worktree.to_path_buf(),
                    basis: range.basis(),
                    path: RepoPath::parse(&anchor.path).map_err(|error| error.to_string())?,
                    old_path: None,
                },
                cancellation,
            )
            .map_err(|error| error.to_string())?;
        match diff {
            crate::repository::DiffContent::Comparison { old, new, .. } => {
                let side = if anchor.side == ReviewDiffSide::Left {
                    old
                } else {
                    new
                };
                side.text().map(str::to_owned).ok_or_else(|| {
                    "The anchor is not a text file. Pick its new line in the gutter.".to_owned()
                })
            }
            _ => Err(
                "The anchor cannot be mapped in this diff. Pick its new line in the gutter.".into(),
            ),
        }
    };
    let (start_line, line) = map_range(
        &read(old_range)?,
        &read(new_range)?,
        anchor.start_line,
        anchor.line,
    )?;
    Ok(Anchor {
        start_line,
        line,
        ..anchor.clone()
    })
}

#[cfg(test)]
mod anchor_tests {
    use super::*;
    #[test]
    fn reanchor_maps_unchanged_surroundings_and_refuses_ambiguous_text() {
        assert_eq!(
            map_range(
                "a\nb\nc\nd\ne\nf\ng\nh\n",
                "new\na\nb\nc\nd\ne\nf\ng\nh\n",
                4,
                5
            ),
            Ok((5, 6))
        );
        assert!(map_range("a\nb\nc\n", "a\nb\nc\na\nb\nc\n", 2, 2).is_err());
        assert!(map_range("a\nb\nc\n", "a\nchanged\nc\n", 2, 2).is_err());
    }
}
