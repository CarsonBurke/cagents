//! PR checkout preparation uses the loaded review's identity. The shared
//! creation dialog and journal remain the owners of new-worktree effects.
use super::*;

impl Cagents {
    pub(super) fn prepare_pr_checkout(
        &mut self,
        card: CardId,
        project: ProjectId,
        target: pull_request_review::CheckoutTarget,
    ) -> Task<Message> {
        let Some(CardRuntime::PullRequests(pane)) = self.cards.get(&card) else {
            return Task::none();
        };
        let generation = pane.review.selection_generation();
        if !pane.review.checkout_target_is_current(&target) {
            self.status = Some(TransientNotice::warning(
                "The reviewed PR branch or source is unavailable or changed. Refresh the review before checking it out.",
            ));
            return Task::none();
        }
        self.status = Some(TransientNotice::success("Preparing PR checkout…"));
        cosmic::task::future(async move {
            let work = target.clone();
            let result = tokio::task::spawn_blocking(move || {
                pull_request_review::checkout::prepare(&work, &CancellationToken::new())
            })
            .await
            .map_err(|error| error.to_string())
            .and_then(|result| result);
            Message::PullRequestCheckoutReady {
                card,
                project,
                generation,
                target,
                result,
            }
        })
    }

    pub(super) fn pr_checkout_ready(
        &mut self,
        card: CardId,
        project: ProjectId,
        generation: u64,
        target: pull_request_review::CheckoutTarget,
        result: Result<pull_request_review::checkout::PreparedCheckout, String>,
    ) -> Task<Message> {
        if !matches!(self.cards.get(&card), Some(CardRuntime::PullRequests(pane))
            if pane.project(&self.sidebar_model) == Some(project)
                && pane.review.selection_generation() == generation
                && pane.review.checkout_target_is_current(&target))
        {
            return Task::none();
        }
        if self.update_prepared {
            if let Some(CardRuntime::PullRequests(pane)) = self.cards.get_mut(&card) {
                pane.review.report_checkout_error(
                    "Checkout preparation paused for the update. Prepare it again after cancelling the update."
                        .into(),
                );
            }
            return Task::none();
        }
        match result {
            Ok(plan) => {
                if let Some(reconciliation) = plan.reconciliation {
                    if let Some(CardRuntime::PullRequests(pane)) = self.cards.get_mut(&card) {
                        pane.review
                            .offer_existing_checkout(target, reconciliation, plan.notice);
                    }
                    return Task::none();
                }
                if plan.existing_worktree.is_some() {
                    if let Some(CardRuntime::PullRequests(pane)) = self.cards.get_mut(&card) {
                        pane.review.report_checkout_error(
                            "Existing checkout has no verified reconciliation plan; prepare it again."
                                .into(),
                        );
                    }
                    return Task::none();
                }
                let task = self.open_worktree_dialog(
                    project,
                    plan.repository,
                    WorktreeDraft {
                        branch: plan.branch_input,
                        base: plan.base,
                        destination: String::new(),
                        ..Default::default()
                    },
                    None,
                );
                if let Some(dialog) = &mut self.worktree_dialog {
                    dialog.set_checkout_notice(plan.notice);
                }
                task
            }
            Err(error) => {
                if let Some(CardRuntime::PullRequests(pane)) = self.cards.get_mut(&card) {
                    pane.review.report_checkout_error(error);
                }
                Task::none()
            }
        }
    }
}
