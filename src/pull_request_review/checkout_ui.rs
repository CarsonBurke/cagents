//! An existing checkout is offered, reconciled on a worker, then opened from
//! a verified receipt. Merely finding a similarly named branch grants nothing.
use super::*;
use crate::repository::{ExistingPrCheckoutPlan, PrCheckoutDisposition, VerifiedPrCheckout};

#[derive(Debug, Clone)]
pub enum Action {
    Confirm,
    Cancel,
    Open,
    Finished {
        generation: u64,
        ticket: u64,
        result: Result<VerifiedPrCheckout, String>,
    },
}

#[derive(Default)]
pub(super) struct Confirmation {
    ticket: u64,
    target: Option<CheckoutTarget>,
    plan: Option<ExistingPrCheckoutPlan>,
    notice: Option<String>,
    cancellation: Option<CancellationToken>,
    completed: Option<VerifiedPrCheckout>,
    error: Option<String>,
    accepted: bool,
}
impl Confirmation {
    pub(super) fn busy(&self) -> bool {
        self.cancellation.is_some()
    }
    pub(super) fn reset(&mut self) {
        if let Some(cancel) = self.cancellation.take() {
            cancel.cancel();
        }
        self.ticket = self.ticket.wrapping_add(1);
        self.target = None;
        self.plan = None;
        self.notice = None;
        self.completed = None;
        self.error = None;
        self.accepted = false;
    }
}

impl State {
    #[cfg(test)]
    pub(crate) fn fixture_existing_checkout_pending(&mut self) -> u64 {
        self.existing_checkout.ticket = self.existing_checkout.ticket.wrapping_add(1);
        self.existing_checkout.accepted = true;
        self.existing_checkout.cancellation = Some(CancellationToken::new());
        self.existing_checkout.ticket
    }

    pub fn checkout_target_is_current(&self, target: &CheckoutTarget) -> bool {
        self.range
            .as_ref()
            .is_some_and(|range| range.head == target.head)
            && self.target.as_ref().is_some_and(|review| {
                review.worktree == target.repository
                    && review.pull_request.number == target.number
                    && review.pull_request.head_ref_name == target.head_ref_name
                    && review.pull_request.head_ref_oid == target.head.as_str()
                    && target
                        .head_repository
                        .as_ref()
                        .zip(review.pull_request.head_repository_key().as_ref())
                        .is_some_and(|(original, current)| original.same_repository(current))
                    && target.pull_request.as_ref()
                        == PullRequestKey::new(review.repository.clone(), target.number)
                            .ok()
                            .as_ref()
            })
    }

    fn existing_checkout_plan_is_current(
        &self,
        target: &CheckoutTarget,
        plan: &ExistingPrCheckoutPlan,
    ) -> bool {
        self.checkout_target_is_current(target)
            && plan.request().expected_head == target.head
            && plan.request().repository == target.repository
            && plan.request().head_branch.as_str() == target.head_ref_name
            && self
                .target
                .as_ref()
                .and_then(|review| review.pull_request.head_repository_key())
                .is_some_and(|source| source.same_repository(&plan.request().head_repository))
    }

    pub fn offer_existing_checkout(
        &mut self,
        target: CheckoutTarget,
        plan: ExistingPrCheckoutPlan,
        notice: String,
    ) {
        if self.existing_checkout.busy() {
            return;
        }
        if !self.existing_checkout_plan_is_current(&target, &plan) {
            self.notice = Some(
                "The PR branch, source or head changed. Refresh the review before checking it out."
                    .into(),
            );
            return;
        }
        self.existing_checkout.reset();
        self.existing_checkout.target = Some(target);
        self.existing_checkout.plan = Some(plan);
        self.existing_checkout.notice = Some(notice);
    }

    pub(super) fn update_existing_checkout(&mut self, action: Action) -> ActionTask {
        match action {
            Action::Cancel => {
                if let Some(cancel) = &self.existing_checkout.cancellation {
                    cancel.cancel();
                    self.existing_checkout.error =
                        Some("Cancellation requested; waiting for the checkout outcome.".into());
                } else {
                    self.existing_checkout.reset();
                }
            }
            Action::Confirm => {
                let Some(target) = self.existing_checkout.target.as_ref() else {
                    return ActionTask::none();
                };
                if self.refreshing
                    || self.existing_checkout.cancellation.is_some()
                    || self.existing_checkout.accepted
                    || !self.checkout_target_is_current(target)
                {
                    return ActionTask::none();
                }
                let Some(plan) = self.existing_checkout.plan.clone() else {
                    return ActionTask::none();
                };
                if !self.existing_checkout_plan_is_current(target, &plan) {
                    return ActionTask::none();
                }
                let cancellation = CancellationToken::new();
                self.existing_checkout.cancellation = Some(cancellation.clone());
                self.existing_checkout.completed = None;
                self.existing_checkout.error = None;
                self.existing_checkout.accepted = true;
                self.existing_checkout.ticket = self.existing_checkout.ticket.wrapping_add(1);
                let ticket = self.existing_checkout.ticket;
                let generation = self.generation;
                return ActionTask::perform(
                    async move {
                        let result = tokio::task::spawn_blocking(move || {
                            RepositoryService::system("git")
                                .reconcile_existing_pr_checkout(&plan, &cancellation)
                                .map_err(|error| error.to_string())
                        })
                        .await
                        .map_err(|error| error.to_string())
                        .and_then(|result| result);
                        Action::Finished {
                            generation,
                            ticket,
                            result,
                        }
                    },
                    |action| super::Action::ExistingCheckout(action),
                );
            }
            Action::Finished {
                generation,
                ticket,
                result,
            } => {
                if generation != self.generation || ticket != self.existing_checkout.ticket {
                    return ActionTask::none();
                }
                let cancelled = self
                    .existing_checkout
                    .cancellation
                    .take()
                    .is_some_and(|token| token.is_cancelled());
                match result {
                    Ok(receipt) => {
                        self.existing_checkout.completed = Some(receipt);
                        self.existing_checkout.error=cancelled.then(||"Checkout reconciliation completed after cancellation was requested. Review the outcome before opening it.".into());
                    }
                    Err(error) => self.existing_checkout.error = Some(error),
                }
            }
            Action::Open => {
                let (Some(target), Some(plan), Some(receipt)) = (
                    &self.existing_checkout.target,
                    &self.existing_checkout.plan,
                    &self.existing_checkout.completed,
                ) else {
                    return ActionTask::none();
                };
                if self.refreshing || !self.existing_checkout_plan_is_current(target, plan) {
                    self.existing_checkout.error = Some(
                        "The checkout no longer matches this review. Prepare checkout again."
                            .into(),
                    );
                    return ActionTask::none();
                }
                let path = match checkout::verified_existing_path(target, plan, receipt) {
                    Ok(path) => path,
                    Err(error) => {
                        self.existing_checkout.error = Some(error);
                        return ActionTask::none();
                    }
                };
                self.existing_checkout.reset();
                self.effects.push(Effect::OpenCheckout(path));
            }
        }
        ActionTask::none()
    }

    pub(super) fn existing_checkout_view(
        &self,
        modal_open: bool,
    ) -> Option<Element<'_, super::Action>> {
        let plan = self.existing_checkout.plan.as_ref()?;
        let busy = self.existing_checkout.cancellation.is_some();
        let current = self
            .existing_checkout
            .target
            .as_ref()
            .is_some_and(|target| self.existing_checkout_plan_is_current(target, plan));
        let action = |action| self.fenced(super::Action::ExistingCheckout(action));
        let mut rows = vec![
            design::caption(
                self.existing_checkout
                    .notice
                    .as_deref()
                    .unwrap_or_else(|| plan.notice()),
            )
            .into(),
        ];
        if !current {
            rows.push(
                design::caption("The PR branch, source or head changed. Prepare checkout again.")
                    .into(),
            );
        }
        if let Some(error) = &self.existing_checkout.error {
            rows.push(design::caption(error).into());
        }
        if self.existing_checkout.accepted && !busy && self.existing_checkout.completed.is_none() {
            rows.push(
                design::caption(
                    "Inspect this outcome, then prepare checkout again before retrying.",
                )
                .into(),
            );
        }
        let primary = if self.existing_checkout.completed.is_some() {
            crate::ui_controls::button::secondary("Open existing worktree").on_press_maybe(
                (!modal_open && !busy && current && !self.refreshing).then(|| action(Action::Open)),
            )
        } else {
            let label = if busy {
                "Reconciling checkout…"
            } else {
                match plan.disposition() {
                    PrCheckoutDisposition::OpenPreservingLocal => "Keep existing local work",
                    PrCheckoutDisposition::RepairTrackingPreservingLocal => {
                        "Configure upstream and open local work"
                    }
                    PrCheckoutDisposition::AlreadyCurrent => "Verify existing checkout",
                    PrCheckoutDisposition::FastForward => "Update checkout",
                    PrCheckoutDisposition::RepairTracking => "Set PR upstream",
                    PrCheckoutDisposition::FastForwardAndRepair => {
                        "Update checkout and set PR upstream"
                    }
                }
            };
            crate::ui_controls::button::secondary(label).on_press_maybe(
                (!modal_open
                    && !busy
                    && !self.refreshing
                    && current
                    && !self.existing_checkout.accepted)
                    .then(|| action(Action::Confirm)),
            )
        };
        rows.push(
            widget::row::with_children(vec![
                primary.into(),
                crate::ui_controls::button::quiet(if busy {
                    "Cancel reconciliation"
                } else {
                    "Dismiss"
                })
                .on_press_maybe((!modal_open).then(|| action(Action::Cancel)))
                .into(),
            ])
            .spacing(space::S)
            .into(),
        );
        Some(widget::column::with_children(rows).spacing(space::S).into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repository::test_fixture::isolated_fixture_case;
    use crate::repository::{ExistingPrCheckoutRequest, PrCheckoutTracking, ValidatedRefName};
    use cosmic::iced::futures::StreamExt as _;

    fn git(directory: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(directory)
            .args([
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "core.fsmonitor=false",
            ])
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_COMMON_DIR")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "Git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    #[test]
    fn existing_checkout_tracking_changes_only_after_confirmation_and_opens_only_after_verified_receipt()
     {
        if isolated_fixture_case(concat!(
            module_path!(),
            "::",
            "existing_checkout_tracking_changes_only_after_confirmation_and_opens_only_after_verified_receipt"
        )) {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let repository = directory.path().join("repository");
        let remote = directory.path().join("remote.git");
        let worktree = directory.path().join("existing");
        git(
            directory.path(),
            &["init", "--bare", remote.to_str().unwrap()],
        );
        git(
            directory.path(),
            &[
                "init",
                "--initial-branch=main",
                repository.to_str().unwrap(),
            ],
        );
        git(&repository, &["config", "user.name", "Fixture"]);
        git(
            &repository,
            &["config", "user.email", "fixture@example.invalid"],
        );
        std::fs::write(repository.join("README"), "Reviewed content\n").unwrap();
        git(&repository, &["add", "README"]);
        git(&repository, &["commit", "-m", "Initial fixture"]);
        git(
            &repository,
            &["push", remote.to_str().unwrap(), "HEAD:refs/heads/feature"],
        );
        git(
            &repository,
            &[
                "worktree",
                "add",
                "--no-track",
                "-b",
                "feature",
                worktree.to_str().unwrap(),
                "HEAD",
            ],
        );
        let url = "https://fixture.invalid/owner/repo.git";
        git(&repository, &["remote", "add", "origin", url]);
        git(
            &repository,
            &[
                "config",
                &format!("url.{}.insteadOf", remote.display()),
                url,
            ],
        );
        git(
            &repository,
            &["config", "fixture.keep", "unrelated configuration"],
        );
        let head = Oid::parse(&git(&repository, &["rev-parse", "HEAD"])).unwrap();
        let key = GitHubRepoKey::new("fixture.invalid", None, "owner", "repo").unwrap();
        let request = ExistingPrCheckoutRequest {
            repository: repository.clone(),
            worktree: worktree.clone(),
            branch: ValidatedBranchName::parse("feature").unwrap(),
            head_repository: key.clone(),
            head_branch: ValidatedBranchName::parse("feature").unwrap(),
            expected_head: head.clone(),
            remote: "origin".into(),
            tracking: PrCheckoutTracking::RemoteBranch,
        };
        let config = std::fs::read(repository.join(".git/config")).unwrap();
        let plan = RepositoryService::system("git")
            .prepare_existing_pr_checkout(&request, &CancellationToken::new())
            .unwrap();
        assert_eq!(plan.disposition(), PrCheckoutDisposition::RepairTracking);
        let target = CheckoutTarget {
            repository: repository.clone(),
            number: 7,
            head: head.clone(),
            pull_request: Some(PullRequestKey::new(key.clone(), 7).unwrap()),
            head_ref_name: "feature".into(),
            head_repository: Some(key.clone()),
        };
        let pr:PullRequest=serde_json::from_value(serde_json::json!({
            "number":7,"title":"Fixture","state":"OPEN","headRefName":"feature","headRefOid":head.as_str(),
            "headRepository":{"name":"repo"},"headRepositoryOwner":{"login":"owner"},"baseRefName":"main","updatedAt":"2026-10-15T00:00:00Z","url":"https://fixture.invalid/owner/repo/pull/7"
        })).unwrap();
        let mut state = State::new(
            Arc::new(crate::github::GitHubService::system("gh")),
            RepositoryService::system("git"),
        );
        state.target = Some(ReviewTarget {
            repository: key,
            worktree: repository.clone(),
            pull_request: Arc::new(pr),
        });
        state.range = Some(crate::repository::PullRequestRange {
            number: 7,
            base_branch: ValidatedBranchName::parse("main").unwrap(),
            head_mirror: ValidatedRefName::parse("refs/cagents/fixture/head").unwrap(),
            base_mirror: ValidatedRefName::parse("refs/cagents/fixture/base").unwrap(),
            head: head.clone(),
            base_tip: head.clone(),
            merge_base: head.clone(),
        });
        state.load = LoadState::Ready;
        state.offer_existing_checkout(target, plan, "Set the existing PR branch upstream.".into());
        assert_eq!(
            std::fs::read(repository.join(".git/config")).unwrap(),
            config
        );
        assert!(
            git(
                &repository,
                &["for-each-ref", "--format=%(upstream)", "refs/heads/feature"]
            )
            .is_empty()
        );
        assert!(
            state.take_effects().is_empty(),
            "offering reconciliation does not open an untracked checkout"
        );
        let task = state.update(super::super::Action::ExistingCheckout(Action::Confirm));
        assert!(state.restart_blocker().is_some());
        assert!(state.take_effects().is_empty());
        let reply = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let mut stream = iced::runtime::task::into_stream(task).unwrap();
                while let Some(action) = stream.next().await {
                    if let iced::runtime::Action::Output(
                        action @ super::super::Action::ExistingCheckout(Action::Finished { .. }),
                    ) = action
                    {
                        return action;
                    }
                }
                panic!("confirmed checkout produced no receipt");
            });
        drop(state.update(reply));
        assert!(
            state.existing_checkout.completed.is_some(),
            "{:?}",
            state.existing_checkout.error
        );
        assert_eq!(
            git(
                &repository,
                &["for-each-ref", "--format=%(upstream)", "refs/heads/feature"]
            ),
            "refs/remotes/origin/feature"
        );
        assert_eq!(
            git(&repository, &["config", "fixture.keep"]),
            "unrelated configuration"
        );
        assert!(
            state.take_effects().is_empty(),
            "the user decides when to open the verified result"
        );
        state.range.as_mut().unwrap().head = Oid::parse(&"b".repeat(40)).unwrap();
        drop(state.update(super::super::Action::ExistingCheckout(Action::Open)));
        assert!(
            state.take_effects().is_empty(),
            "a moved review cannot open the earlier receipt"
        );
        state.range.as_mut().unwrap().head = head;
        drop(state.update(super::super::Action::ExistingCheckout(Action::Open)));
        assert_eq!(
            state.take_effects(),
            vec![Effect::OpenCheckout(worktree.canonicalize().unwrap())]
        );
        assert!(state.restart_blocker().is_none());
        std::fs::write(worktree.join("README"), "Reviewed content\nA local fix\n").unwrap();
        git(&worktree, &["add", "README"]);
        git(&worktree, &["commit", "-m", "Fix after checkout"]);
        git(&worktree, &["push"]);
        assert_eq!(
            git(&remote, &["rev-parse", "refs/heads/feature"]),
            git(&worktree, &["rev-parse", "HEAD"]),
            "ordinary git push must publish the existing checkout's fix to its PR head branch"
        );
    }

    #[test]
    fn preserving_ahead_and_dirty_local_work_requires_confirmation_and_a_matching_typed_receipt() {
        if isolated_fixture_case(concat!(
            module_path!(),
            "::",
            "preserving_ahead_and_dirty_local_work_requires_confirmation_and_a_matching_typed_receipt"
        )) {
            return;
        }
        preserve_ahead_and_dirty_local_work(false);
    }

    #[test]
    fn configuring_missing_upstream_while_preserving_local_work_requires_explicit_confirmation() {
        if isolated_fixture_case(concat!(
            module_path!(),
            "::",
            "configuring_missing_upstream_while_preserving_local_work_requires_explicit_confirmation"
        )) {
            return;
        }
        preserve_ahead_and_dirty_local_work(true);
    }

    fn preserve_ahead_and_dirty_local_work(repair_tracking: bool) {
        let directory = tempfile::tempdir().unwrap();
        let repository = directory.path().join("repository");
        let remote = directory.path().join("remote.git");
        git(
            directory.path(),
            &["init", "--bare", remote.to_str().unwrap()],
        );
        git(
            directory.path(),
            &[
                "init",
                "--initial-branch=main",
                repository.to_str().unwrap(),
            ],
        );
        git(&repository, &["config", "user.name", "Fixture"]);
        git(
            &repository,
            &["config", "user.email", "fixture@example.invalid"],
        );
        std::fs::write(repository.join("README"), "Reviewed content\n").unwrap();
        git(&repository, &["add", "README"]);
        git(&repository, &["commit", "-m", "Reviewed fixture"]);
        let reviewed = Oid::parse(&git(&repository, &["rev-parse", "HEAD"])).unwrap();
        git(&repository, &["checkout", "-b", "feature"]);
        let url = "https://fixture.invalid/owner/repo.git";
        git(&repository, &["remote", "add", "origin", url]);
        git(
            &repository,
            &[
                "config",
                &format!("url.{}.insteadOf", remote.display()),
                url,
            ],
        );
        git(
            &repository,
            &["push", "--set-upstream", "origin", "feature"],
        );
        if repair_tracking {
            git(
                &repository,
                &["config", "--remove-section", "branch.feature"],
            );
        }
        std::fs::write(repository.join("README"), "A committed local fix\n").unwrap();
        git(&repository, &["add", "README"]);
        git(&repository, &["commit", "-m", "Unpublished local fix"]);
        let local = Oid::parse(&git(&repository, &["rev-parse", "HEAD"])).unwrap();
        std::fs::write(repository.join("README"), "Staged local work\n").unwrap();
        git(&repository, &["add", "README"]);
        std::fs::write(repository.join("README"), "Unstaged local work\n").unwrap();
        let config = std::fs::read(repository.join(".git/config")).unwrap();
        let config_values = git(&repository, &["config", "--local", "--list"]);
        let index = std::fs::read(repository.join(".git/index")).unwrap();
        let key = GitHubRepoKey::new("fixture.invalid", None, "owner", "repo").unwrap();
        let request = ExistingPrCheckoutRequest {
            repository: repository.clone(),
            worktree: repository.clone(),
            branch: ValidatedBranchName::parse("feature").unwrap(),
            head_repository: key.clone(),
            head_branch: ValidatedBranchName::parse("feature").unwrap(),
            expected_head: reviewed.clone(),
            remote: "origin".into(),
            tracking: PrCheckoutTracking::RemoteBranch,
        };
        let service = RepositoryService::system("git");
        let cancel = CancellationToken::new();
        assert!(
            service
                .prepare_existing_pr_checkout(&request, &cancel)
                .is_err()
        );
        let plan = checkout::prepare_existing_confirmation(&service, &request, &cancel).unwrap();
        let disposition = if repair_tracking {
            PrCheckoutDisposition::RepairTrackingPreservingLocal
        } else {
            PrCheckoutDisposition::OpenPreservingLocal
        };
        assert_eq!(plan.disposition(), disposition);
        assert_eq!(plan.local_head(), &local);
        let target = CheckoutTarget {
            repository: repository.clone(),
            number: 7,
            head: reviewed.clone(),
            pull_request: Some(PullRequestKey::new(key.clone(), 7).unwrap()),
            head_ref_name: "feature".into(),
            head_repository: Some(key.clone()),
        };
        let pr: PullRequest = serde_json::from_value(serde_json::json!({
            "number":7,"title":"Fixture","state":"OPEN","headRefName":"feature","headRefOid":reviewed.as_str(),
            "headRepository":{"name":"repo"},"headRepositoryOwner":{"login":"owner"},"baseRefName":"main","updatedAt":"2026-10-15T00:00:00Z","url":"https://fixture.invalid/owner/repo/pull/7"
        })).unwrap();
        let mut state = State::new(
            Arc::new(crate::github::GitHubService::system("gh")),
            RepositoryService::system("git"),
        );
        state.target = Some(ReviewTarget {
            repository: key,
            worktree: repository.clone(),
            pull_request: Arc::new(pr),
        });
        state.range = Some(crate::repository::PullRequestRange {
            number: 7,
            base_branch: ValidatedBranchName::parse("main").unwrap(),
            head_mirror: ValidatedRefName::parse("refs/cagents/fixture/head").unwrap(),
            base_mirror: ValidatedRefName::parse("refs/cagents/fixture/base").unwrap(),
            head: reviewed.clone(),
            base_tip: reviewed.clone(),
            merge_base: reviewed,
        });
        state.load = LoadState::Ready;
        state.offer_existing_checkout(target.clone(), plan.clone(), plan.notice().into());
        assert_eq!(
            std::fs::read(repository.join(".git/config")).unwrap(),
            config,
            "offering the explicit upstream repair does not authorize configuration writes"
        );
        drop(state.update(super::super::Action::ExistingCheckout(Action::Open)));
        assert!(
            state.take_effects().is_empty(),
            "preparing or opening before confirmation grants no navigation"
        );
        let original_pr = Arc::clone(&state.target.as_ref().unwrap().pull_request);
        for source_change in [false, true] {
            let current = Arc::make_mut(&mut state.target.as_mut().unwrap().pull_request);
            if source_change {
                current.head_repository_owner.as_mut().unwrap().login = "different-owner".into();
            } else {
                current.head_ref_name = "renamed-branch".into();
            }
            drop(state.update(super::super::Action::ExistingCheckout(Action::Confirm)));
            assert!(
                !state.existing_checkout.busy(),
                "known branch/source changes cannot authorize an old plan at the same OID"
            );
            state.target.as_mut().unwrap().pull_request = Arc::clone(&original_pr);
        }
        let task = state.update(super::super::Action::ExistingCheckout(Action::Confirm));
        let reply = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let mut stream = iced::runtime::task::into_stream(task).unwrap();
                while let Some(action) = stream.next().await {
                    if let iced::runtime::Action::Output(
                        action @ super::super::Action::ExistingCheckout(Action::Finished { .. }),
                    ) = action
                    {
                        return action;
                    }
                }
                panic!("confirmation produced no receipt");
            });
        drop(state.update(reply));
        assert!(
            state.existing_checkout.completed.is_some(),
            "checkout did not produce a verified receipt: {:?}",
            state.existing_checkout.error
        );
        let receipt = state.existing_checkout.completed.as_ref().unwrap();
        assert_eq!(receipt.head(), &local);
        assert_eq!(receipt.disposition(), disposition);
        let mut writer_request = request;
        writer_request.expected_head = local.clone();
        let writer_plan = service
            .prepare_existing_pr_checkout(&writer_request, &cancel)
            .unwrap();
        let mut writer_target = target;
        writer_target.head = local.clone();
        assert!(
            checkout::verified_existing_path(&writer_target, &writer_plan, receipt).is_err(),
            "a preserve-local receipt cannot masquerade as a strict writer receipt even with the same head"
        );
        assert!(state.take_effects().is_empty());
        Arc::make_mut(&mut state.target.as_mut().unwrap().pull_request).head_ref_name =
            "renamed-branch".into();
        drop(state.update(super::super::Action::ExistingCheckout(Action::Open)));
        assert!(
            state.take_effects().is_empty(),
            "a verified receipt cannot override a newly observed PR branch identity"
        );
        state.target.as_mut().unwrap().pull_request = original_pr;
        drop(state.update(super::super::Action::ExistingCheckout(Action::Open)));
        assert_eq!(
            state.take_effects(),
            vec![Effect::OpenCheckout(repository.canonicalize().unwrap())]
        );
        assert_eq!(git(&repository, &["rev-parse", "HEAD"]), local.as_str());
        if repair_tracking {
            assert_eq!(
                git(
                    &repository,
                    &["config", "--local", "--get", "branch.feature.remote"]
                ),
                "origin"
            );
            assert_eq!(
                git(
                    &repository,
                    &["config", "--local", "--get", "branch.feature.merge"]
                ),
                "refs/heads/feature"
            );
            let after = git(&repository, &["config", "--local", "--list"]);
            let unrelated = after
                .lines()
                .filter(|line| {
                    !line.starts_with("branch.feature.remote=")
                        && !line.starts_with("branch.feature.merge=")
                })
                .collect::<Vec<_>>()
                .join("\n");
            assert_eq!(unrelated, config_values);
        } else {
            assert_eq!(
                std::fs::read(repository.join(".git/config")).unwrap(),
                config
            );
        }
        assert_eq!(std::fs::read(repository.join(".git/index")).unwrap(), index);
        assert_eq!(
            std::fs::read_to_string(repository.join("README")).unwrap(),
            "Unstaged local work\n"
        );
    }

    #[test]
    fn stale_checkout_receipts_and_cancel_requests_do_not_retire_an_accepted_new_operation() {
        let mut state = State::new(
            Arc::new(crate::github::GitHubService::system("gh")),
            RepositoryService::system("git"),
        );
        let ticket = state.fixture_existing_checkout_pending();
        let generation = state.generation;
        drop(
            state.update(super::super::Action::ExistingCheckout(Action::Finished {
                generation,
                ticket: ticket.wrapping_sub(1),
                result: Err("older checkout".into()),
            })),
        );
        assert!(state.existing_checkout.busy());
        assert!(state.restart_blocker().is_some());
        drop(state.update(super::super::Action::ExistingCheckout(Action::Cancel)));
        assert!(
            state
                .existing_checkout
                .cancellation
                .as_ref()
                .unwrap()
                .is_cancelled()
        );
        assert!(
            state.restart_blocker().is_some(),
            "a cancellation request does not certify Git settlement"
        );
        drop(
            state.update(super::super::Action::ExistingCheckout(Action::Finished {
                generation,
                ticket,
                result: Err("Current checkout preserved; reconciliation refused.".into()),
            })),
        );
        assert!(!state.existing_checkout.busy());
        assert!(state.restart_blocker().is_none());
        assert!(
            state
                .existing_checkout
                .error
                .as_deref()
                .unwrap()
                .contains("preserved")
        );
        assert!(
            state.take_effects().is_empty(),
            "refused or stale receipts never open a worktree"
        );
    }
}
