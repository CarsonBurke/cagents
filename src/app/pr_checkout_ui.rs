//! PR checkout preparation uses the loaded review's identity. The shared
//! creation dialog and journal remain the owners of new-worktree effects.
use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrCheckoutOrigin {
    pub(super) card: CardId,
    pub(super) project: ProjectId,
    pub(super) generation: u64,
    pub(super) target: pull_request_review::CheckoutTarget,
    pub(super) consent: crate::repository::PrWorktreeCreationConsent,
    pub(super) notice: String,
}

impl Cagents {
    pub(super) fn review_checkout_blocker(&self, card: CardId) -> Option<String> {
        if let Some(CardRuntime::PullRequests(pane)) = self.cards.get(&card)
            && let Some(reason) = pane.review.restart_blocker()
        {
            return Some(reason.to_owned());
        }
        self.worktree_operations
            .values()
            .any(|operation| operation.pr_creation_running(card))
            .then(|| {
                "Wait for the PR worktree creation to settle before closing its review.".into()
            })
    }

    pub(super) fn pr_creation_origin_is_current(&self, origin: &PrCheckoutOrigin) -> bool {
        !self.update_prepared
            && !self.project_ui.closing.contains_key(&origin.project)
            && self.sidebar_model.project(origin.project).is_some()
            && matches!(self.cards.get(&origin.card), Some(CardRuntime::PullRequests(pane))
                if pane.project(&self.sidebar_model) == Some(origin.project)
                    && pane.review.selection_generation() == origin.generation
                    && pane.review.checkout_target_is_current(&origin.target))
    }

    pub(super) fn cancel_stale_pr_creations(&mut self) {
        let stale = self
            .worktree_operations
            .iter()
            .filter_map(|(id, operation)| {
                operation
                    .pr_origin()
                    .filter(|origin| !self.pr_creation_origin_is_current(origin))
                    .map(|_| *id)
            })
            .collect::<Vec<_>>();
        for id in stale {
            drop(self.cancel_worktree_operation(id));
        }
    }

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
                let Some(consent) = plan.creation_consent else {
                    if let Some(CardRuntime::PullRequests(pane)) = self.cards.get_mut(&card) {
                        pane.review.report_checkout_error(
                            "New PR checkout has no reviewed creation consent. Prepare it again."
                                .into(),
                        );
                    }
                    return Task::none();
                };
                let origin = PrCheckoutOrigin {
                    card,
                    project,
                    generation,
                    target,
                    consent,
                    notice: plan.notice.clone(),
                };
                let task = self.open_worktree_dialog(
                    project,
                    plan.repository,
                    WorktreeDraft {
                        branch: plan.branch_input,
                        base: plan.base,
                        destination: String::new(),
                        pr_checkout: Some(origin),
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
