use super::*;
use cagents::{
    model::OperationId,
    persistence::StateStore,
    pull_request_review::CheckoutTarget,
    repository::{
        CreateWorktreeRequest, PrCheckoutDisposition, VerifiedPrCheckout, WorktreeInput,
        WorktreeOperationReport, WorktreeOperationState,
    },
};

#[derive(Clone)]
pub(crate) struct Journal(Arc<StateStore>);
impl std::fmt::Debug for Journal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CheckoutJournal")
    }
}

#[derive(Debug, Clone)]
pub(crate) enum Action {
    JournalLoaded(Result<(Journal, Vec<WorktreeOperationReport>), String>),
    History,
    OpenRecorded(OperationId),
    Branch(String),
    Destination(String),
    Resolve,
    Resolved(OperationId, Result<CreateWorktreeRequest, String>),
    Prepared(
        OperationId,
        Result<cagents::pull_request_review::checkout::PreparedCheckout, String>,
    ),
    Create,
    ReconcileExisting,
    ExistingFinished(OperationId, Result<VerifiedPrCheckout, String>),
    Finished(OperationId, Result<WorktreeOperationReport, String>),
    Cancel,
    Edit,
    Open(PathBuf),
}

pub(super) struct Dialog {
    target: CheckoutTarget,
    review_owner: Option<(CardId, u64, pull_request_review::ReviewTarget)>,
    branch: String,
    destination: String,
    id: OperationId,
    request: Option<CreateWorktreeRequest>,
    cancellation: Option<CancellationToken>,
    error: Option<String>,
    completed: Option<PathBuf>,
    existing_receipt: Option<VerifiedPrCheckout>,
    accepted: bool,
    prepared: Option<cagents::pull_request_review::checkout::PreparedCheckout>,
}
impl Dialog {
    pub(super) fn busy(&self) -> bool {
        self.cancellation.is_some()
    }
    #[cfg(test)]
    pub(super) fn fixture_pending_existing(&mut self) -> OperationId {
        self.accepted = true;
        self.cancellation = Some(CancellationToken::new());
        self.id
    }
    #[cfg(test)]
    pub(super) fn fixture_prepared(
        &mut self,
        prepared: cagents::pull_request_review::checkout::PreparedCheckout,
    ) -> OperationId {
        self.prepared = Some(prepared);
        self.id
    }
    #[cfg(test)]
    pub(super) fn completed_for_test(&self) -> Option<&Path> {
        self.completed.as_deref()
    }
    #[cfg(test)]
    pub(super) fn cancellation_for_test(&self) -> Option<CancellationToken> {
        self.cancellation.clone()
    }
    #[cfg(test)]
    pub(super) fn retained_receipt_for_test(&self) -> Option<&VerifiedPrCheckout> {
        self.existing_receipt.as_ref()
    }
    #[cfg(test)]
    pub(super) fn error_for_test(&self) -> Option<&str> {
        self.error.as_deref()
    }
}

impl Editor {
    pub(super) fn load_checkout_journal(&self) -> Task<Message> {
        let path = self
            .settings_path
            .with_file_name("worktree-operations.sqlite3");
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || {
                let boot = cagents::session::current_boot_id().map_err(|e| e.to_string())?;
                let store = Arc::new(StateStore::open(path, boot).map_err(|e| e.to_string())?);
                let reports = RepositoryService::system("git")
                    .rehydrate_worktree_operations(&store)
                    .map_err(|e| e.to_string())?;
                Ok((Journal(store), reports))
            })
            .await
            .map_err(|e| e.to_string())
            .and_then(|result| result);
            Message::Checkout(Action::JournalLoaded(result))
        })
    }

    #[cfg(test)]
    pub(super) fn begin_checkout(&mut self, target: CheckoutTarget) {
        self.begin_checkout_owned(target, None);
    }

    pub(super) fn begin_review_checkout(&mut self, card: CardId, target: CheckoutTarget) {
        let Some(tab) = self.tab(card) else {
            return;
        };
        let Some(review) = tab.review.target().cloned() else {
            return;
        };
        let owner = (card, tab.review.selection_generation(), review);
        self.begin_checkout_owned(target, Some(owner));
        self.invalidate_stale_checkout();
    }

    fn begin_checkout_owned(
        &mut self,
        target: CheckoutTarget,
        review_owner: Option<(CardId, u64, pull_request_review::ReviewTarget)>,
    ) {
        if self.modal_open() {
            return;
        }
        self.checkout_dialog = Some(Dialog {
            branch: target.suggested_branch(),
            target,
            review_owner,
            destination: String::new(),
            id: OperationId::new(),
            request: None,
            cancellation: None,
            error: None,
            completed: None,
            existing_receipt: None,
            accepted: false,
            prepared: None,
        });
    }

    fn checkout_origin_is_current(&self) -> bool {
        let Some(dialog) = self.checkout_dialog.as_ref() else {
            return false;
        };
        let Some((card, generation, original)) = dialog.review_owner.as_ref() else {
            return dialog.target.pull_request.is_none();
        };
        let Some(tab) = self.tab(*card) else {
            return false;
        };
        let Some(current) = tab.review.target() else {
            return false;
        };
        *generation == tab.review.selection_generation()
            && original.repository.same_repository(&current.repository)
            && original.worktree == current.worktree
            && current.worktree == dialog.target.repository
            && original.pull_request.id == current.pull_request.id
            && original.pull_request.number == current.pull_request.number
            && current.pull_request.number == dialog.target.number
            && original.pull_request.head_ref_name == current.pull_request.head_ref_name
            && current.pull_request.head_ref_name == dialog.target.head_ref_name
            && original.pull_request.head_ref_oid == current.pull_request.head_ref_oid
            && current.pull_request.head_ref_oid == dialog.target.head.as_str()
            && tab
                .review
                .range()
                .is_some_and(|range| range.head == dialog.target.head)
            && original
                .pull_request
                .head_repository_key()
                .zip(current.pull_request.head_repository_key())
                .is_some_and(|(original, current)| original.same_repository(&current))
            && dialog.target.pull_request.as_ref().is_some_and(|key| {
                key.repository.same_repository(&current.repository)
                    && key.number == current.pull_request.number
            })
    }

    fn checkout_plan_is_current(
        &self,
        prepared: &cagents::pull_request_review::checkout::PreparedCheckout,
    ) -> bool {
        if !self.checkout_origin_is_current() {
            return false;
        }
        let dialog = self.checkout_dialog.as_ref().unwrap();
        prepared.repository == dialog.target.repository
            && prepared.reconciliation.as_ref().is_none_or(|plan| {
                let request = plan.request();
                request.expected_head == dialog.target.head
                    && request.repository == dialog.target.repository
                    && request.head_branch.as_str() == dialog.target.head_ref_name
                    && dialog
                        .review_owner
                        .as_ref()
                        .is_some_and(|(_, _, original)| {
                            original
                                .pull_request
                                .belongs_to_head(&request.head_repository)
                        })
            })
    }

    pub(super) fn invalidate_stale_checkout(&mut self) {
        if self.checkout_dialog.is_some() && !self.checkout_origin_is_current() {
            let dialog = self.checkout_dialog.as_mut().unwrap();
            if let Some(token) = &dialog.cancellation {
                token.cancel();
            }
            dialog.completed = None;
            dialog.error = Some("The owning PR branch, source or head changed. Close this dialog and prepare checkout again.".into());
        }
    }

    pub(super) fn checkout_action(&mut self, action: Action) -> Task<Message> {
        let origin_current = self.checkout_origin_is_current();
        if matches!(
            &action,
            Action::Resolve | Action::Create | Action::ReconcileExisting | Action::Open(_)
        ) && !origin_current
        {
            self.invalidate_stale_checkout();
            return Task::none();
        }
        match action {
            Action::JournalLoaded(result) => match result {
                Ok((journal, reports)) => {
                    self.checkout_error = None;
                    self.checkout_journal = Some(journal);
                    let unresolved = reports.iter().filter(|r| {
                        !matches!(
                            r.state,
                            WorktreeOperationState::Created | WorktreeOperationState::NotCreated
                        )
                    });
                    let messages = unresolved
                        .map(|r| format!("Checkout {}: {}", r.operation_id, r.detail))
                        .collect::<Vec<_>>();
                    if !messages.is_empty() {
                        self.notice = Some(messages.join("\n"));
                    }
                    self.checkout_history = reports;
                    self.trim_checkout_history();
                }
                Err(error) => {
                    self.notice = Some(format!("Worktree creation unavailable: {error}"));
                    self.checkout_error = Some(error);
                }
            },
            Action::History => {
                if !self.modal_open() {
                    self.checkout_history_open = true;
                }
            }
            Action::OpenRecorded(id) => {
                if self.checkout_history_open {
                    if let Some(path) = self
                        .checkout_history
                        .iter()
                        .find(|r| {
                            r.operation_id == id && r.state == WorktreeOperationState::Created
                        })
                        .and_then(|r| r.request.as_ref())
                        .map(|r| r.destination.clone())
                    {
                        self.checkout_history_open = false;
                        self.selection_epoch = self.selection_epoch.wrapping_add(1);
                        return Self::open(path, self.selection_epoch);
                    }
                }
            }
            Action::Open(path) => {
                if self.checkout_dialog.as_ref().is_some_and(|d| {
                    d.completed.as_ref() == Some(&path)
                        && d.prepared
                            .as_ref()
                            .and_then(|plan| plan.reconciliation.as_ref())
                            .is_none_or(|plan| {
                                d.existing_receipt.as_ref().is_some_and(|receipt| {
                                    cagents::pull_request_review::checkout::verified_existing_path(
                                        &d.target, plan, receipt,
                                    )
                                    .as_ref()
                                        == Ok(&path)
                                })
                            })
                }) {
                    self.checkout_dialog = None;
                    self.selection_epoch = self.selection_epoch.wrapping_add(1);
                    return Self::open(path, self.selection_epoch);
                }
            }
            Action::Branch(value) => {
                if let Some(dialog) = self
                    .checkout_dialog
                    .as_mut()
                    .filter(|d| !d.busy() && d.request.is_none() && d.target.pull_request.is_none())
                {
                    dialog.branch = value;
                    dialog.error = None;
                }
            }
            Action::Destination(value) => {
                if let Some(dialog) = self
                    .checkout_dialog
                    .as_mut()
                    .filter(|d| !d.busy() && d.request.is_none())
                {
                    dialog.destination = value;
                    dialog.error = None;
                }
            }
            Action::Resolve => {
                if self.checkout_journal.is_none() {
                    return Task::none();
                }
                let Some(dialog) = self
                    .checkout_dialog
                    .as_mut()
                    .filter(|d| !d.busy() && d.request.is_none() && !d.accepted)
                else {
                    return Task::none();
                };
                let cancellation = CancellationToken::new();
                dialog.cancellation = Some(cancellation.clone());
                dialog.error = None;
                let id = dialog.id;
                if dialog.prepared.is_none() {
                    let target = dialog.target.clone();
                    return cosmic::task::future(async move {
                        let result = tokio::task::spawn_blocking(move || {
                            cagents::pull_request_review::checkout::prepare(&target, &cancellation)
                        })
                        .await
                        .map_err(|error| error.to_string())
                        .and_then(|result| result);
                        Message::Checkout(Action::Prepared(id, result))
                    });
                }
                let input = WorktreeInput {
                    repository: dialog.target.repository.clone(),
                    input: dialog.branch.clone(),
                    base: dialog.prepared.as_ref().map_or_else(
                        || dialog.target.head.as_str().to_owned(),
                        |plan| plan.base.clone(),
                    ),
                    managed_directory: self.settings.managed_worktree_directory.clone(),
                    destination: dialog.destination.clone(),
                    operation_id: id,
                };
                let consent = dialog
                    .prepared
                    .as_ref()
                    .and_then(|plan| plan.creation_consent.clone());
                return cosmic::task::future(async move {
                    let result = tokio::task::spawn_blocking(move || {
                        let service = RepositoryService::system("git");
                        match consent {
                            Some(consent) => service.resolve_worktree_request_for_pr(
                                &input,
                                &consent,
                                &cancellation,
                            ),
                            None => service.resolve_worktree_request(&input, &cancellation),
                        }
                        .map_err(|e| e.to_string())
                    })
                    .await
                    .map_err(|e| e.to_string())
                    .and_then(|r| r);
                    Message::Checkout(Action::Resolved(id, result))
                });
            }
            Action::Prepared(id, result) => {
                let plan_current = result
                    .as_ref()
                    .is_ok_and(|plan| self.checkout_plan_is_current(plan));
                let Some(dialog) = self
                    .checkout_dialog
                    .as_mut()
                    .filter(|dialog| dialog.id == id)
                else {
                    return Task::none();
                };
                let cancelled = dialog
                    .cancellation
                    .take()
                    .is_some_and(|token| token.is_cancelled());
                if cancelled || !origin_current {
                    dialog.error =
                        Some("Cancelled or owning PR changed before preparing checkout.".into());
                    return Task::none();
                }
                match result {
                    Ok(plan) => {
                        if !plan_current {
                            dialog.error = Some("Prepared checkout no longer matches the owning PR. Prepare it again.".into());
                            return Task::none();
                        }
                        if plan.existing_worktree.is_some() && plan.reconciliation.is_none() {
                            dialog.error=Some("Existing checkout has no verified reconciliation plan; prepare it again.".into());
                            return Task::none();
                        }
                        dialog.branch = plan.branch_input.clone();
                        let existing = plan.reconciliation.is_some();
                        dialog.prepared = Some(plan);
                        if !existing {
                            return self.checkout_action(Action::Resolve);
                        }
                    }
                    Err(error) => dialog.error = Some(error),
                }
            }
            Action::Resolved(id, result) => {
                if let Some(dialog) = self.checkout_dialog.as_mut().filter(|d| d.id == id) {
                    let cancelled = dialog
                        .cancellation
                        .take()
                        .is_some_and(|token| token.is_cancelled());
                    if cancelled || !origin_current {
                        dialog.error = Some(
                            "Cancelled or owning PR changed before creating a worktree.".into(),
                        );
                    } else {
                        match result {
                            Ok(request) => dialog.request = Some(request),
                            Err(error) => dialog.error = Some(error),
                        }
                    }
                }
            }
            Action::ReconcileExisting => {
                if !self
                    .checkout_dialog
                    .as_ref()
                    .and_then(|dialog| dialog.prepared.as_ref())
                    .is_some_and(|plan| self.checkout_plan_is_current(plan))
                {
                    return Task::none();
                }
                let Some(dialog) = self
                    .checkout_dialog
                    .as_mut()
                    .filter(|dialog| !dialog.busy() && !dialog.accepted)
                else {
                    return Task::none();
                };
                let Some(plan) = dialog
                    .prepared
                    .as_ref()
                    .and_then(|prepared| prepared.reconciliation.clone())
                else {
                    return Task::none();
                };
                if plan.request().expected_head != dialog.target.head {
                    return Task::none();
                }
                dialog.accepted = true;
                dialog.error = None;
                let cancellation = CancellationToken::new();
                dialog.cancellation = Some(cancellation.clone());
                let id = dialog.id;
                return cosmic::task::future(async move {
                    let result = tokio::task::spawn_blocking(move || {
                        RepositoryService::system("git")
                            .reconcile_existing_pr_checkout(&plan, &cancellation)
                            .map_err(|error| error.to_string())
                    })
                    .await
                    .map_err(|error| error.to_string())
                    .and_then(|result| result);
                    Message::Checkout(Action::ExistingFinished(id, result))
                });
            }
            Action::ExistingFinished(id, result) => {
                let Some(dialog) = self
                    .checkout_dialog
                    .as_mut()
                    .filter(|dialog| dialog.id == id)
                else {
                    return Task::none();
                };
                dialog.cancellation = None;
                if let Ok(receipt) = &result {
                    dialog.existing_receipt = Some(receipt.clone());
                }
                if !origin_current {
                    dialog.completed = None;
                    dialog.error = Some("Checkout settled after its owning PR changed. Its result was retained; refresh the review before opening it.".into());
                    return Task::none();
                }
                match result {
                    Ok(receipt) => {
                        let path = dialog
                            .prepared
                            .as_ref()
                            .and_then(|prepared| prepared.reconciliation.as_ref())
                            .ok_or_else(|| {
                                "The checkout plan was replaced. Prepare checkout again.".to_owned()
                            })
                            .and_then(|plan| {
                                cagents::pull_request_review::checkout::verified_existing_path(
                                    &dialog.target,
                                    plan,
                                    &receipt,
                                )
                            });
                        match path {
                            Ok(path) => dialog.completed = Some(path),
                            Err(error) => dialog.error = Some(error),
                        }
                    }
                    Err(error) => dialog.error = Some(error),
                }
            }
            Action::Create => {
                let Some(journal) = self.checkout_journal.clone() else {
                    return Task::none();
                };
                let Some(dialog) = self
                    .checkout_dialog
                    .as_mut()
                    .filter(|d| !d.busy() && !d.accepted)
                else {
                    return Task::none();
                };
                let Some(request) = dialog.request.clone() else {
                    return Task::none();
                };
                let consent = dialog
                    .prepared
                    .as_ref()
                    .and_then(|plan| plan.creation_consent.clone());
                if dialog.target.pull_request.is_some() && consent.is_none() {
                    dialog.error = Some(
                        "The reviewed creation consent is missing; prepare checkout again.".into(),
                    );
                    return Task::none();
                }
                dialog.accepted = true;
                let cancellation = CancellationToken::new();
                dialog.cancellation = Some(cancellation.clone());
                dialog.error = None;
                let id = dialog.id;
                return cosmic::task::future(async move {
                    let result = tokio::task::spawn_blocking(move || {
                        cagents::pull_request_review::checkout::create(
                            &journal.0,
                            &request,
                            consent.as_ref(),
                            &cancellation,
                        )
                    })
                    .await
                    .map_err(|e| e.to_string());
                    Message::Checkout(Action::Finished(id, result))
                });
            }
            Action::Finished(id, result) => {
                if let Ok(report) = &result {
                    self.checkout_history
                        .retain(|r| r.operation_id != report.operation_id);
                    self.checkout_history.push(report.clone());
                    self.trim_checkout_history();
                }
                if let Some(dialog) = self.checkout_dialog.as_mut().filter(|d| d.id == id) {
                    dialog.cancellation = None;
                    match result {
                        Ok(report)
                            if report.state == WorktreeOperationState::Created
                                && origin_current =>
                        {
                            dialog.completed = report.request.map(|r| r.destination);
                        }
                        Ok(report) if !origin_current => {
                            dialog.error = Some(format!(
                                "Checkout settled after its owning PR changed. {} Reopen the review before opening its result.",
                                report.detail
                            ))
                        }
                        Ok(report) => dialog.error = Some(report.detail),
                        Err(error) => {
                            dialog.error = Some(format!(
                                "Checkout outcome needs reconciliation: {error}. Reopen Ceditor to inspect the durable journal."
                            ))
                        }
                    }
                    // A new click must never silently replay an accepted job.
                    dialog.request = None;
                }
            }
            Action::Edit => {
                if let Some(dialog) = self
                    .checkout_dialog
                    .as_mut()
                    .filter(|d| !d.busy() && !d.accepted)
                {
                    dialog.request = None;
                    dialog.id = OperationId::new();
                }
            }
            Action::Cancel => {
                if let Some(token) = self
                    .checkout_dialog
                    .as_ref()
                    .and_then(|d| d.cancellation.as_ref())
                {
                    token.cancel();
                } else {
                    self.checkout_dialog = None;
                    self.checkout_history_open = false;
                }
            }
        }
        Task::none()
    }

    fn trim_checkout_history(&mut self) {
        self.checkout_history
            .sort_by_key(|r| std::cmp::Reverse(r.created_at_unix_ms));
        self.checkout_history.truncate(20);
    }

    pub(super) fn checkout_view(&self) -> Option<Element<'_, Message>> {
        if self.checkout_history_open {
            let mut rows: Vec<Element<'_, Message>> =
                vec![widget::text::title3("Recent worktrees").into()];
            if let Some(error) = &self.checkout_error {
                rows.push(
                    widget::text::body(format!("Worktree history unavailable: {error}")).into(),
                );
            } else if self.checkout_journal.is_none() {
                rows.push(widget::text::body("Loading worktree history…").into());
            } else if self.checkout_history.is_empty() {
                rows.push(widget::text::body("No worktree operations yet.").into());
            }
            for report in &self.checkout_history {
                if let Some(request) = &report.request {
                    rows.push(
                        widget::text::body(format!(
                            "{} · {}",
                            request.branch.as_str(),
                            request.destination.display()
                        ))
                        .into(),
                    );
                }
                rows.push(design::caption(&report.detail).into());
                if report.state == WorktreeOperationState::Created && report.request.is_some() {
                    rows.push(
                        widget::button::standard("Open worktree")
                            .on_press(Message::Checkout(Action::OpenRecorded(report.operation_id)))
                            .into(),
                    );
                }
            }
            rows.push(
                widget::button::standard("Close")
                    .on_press(Message::Checkout(Action::Cancel))
                    .into(),
            );
            return Some(dialog_overlay(
                dialog_surface(
                    widget::container(widget::scrollable(
                        widget::column::with_children(rows).spacing(12),
                    ))
                    .max_height(480.0),
                    640.0,
                ),
                Message::Checkout(Action::Cancel),
            ));
        }
        let dialog = self.checkout_dialog.as_ref()?;
        let action = |a| Message::Checkout(a);
        let mut body = vec![
            widget::text::title3(
                if dialog
                    .prepared
                    .as_ref()
                    .is_some_and(|prepared| prepared.reconciliation.is_some())
                {
                    "Open existing PR worktree"
                } else {
                    "Create PR worktree"
                },
            )
            .into(),
            design::caption(format!(
                "PR #{} · {}",
                dialog.target.number,
                dialog.target.head.as_str()
            ))
            .into(),
        ];
        if let Some(plan) = &dialog.prepared {
            body.push(design::caption(&plan.notice).into());
        }
        if let Some(path) = &dialog.completed {
            body.push(widget::text::body(format!("Worktree ready at {}", path.display())).into());
            body.push(
                widget::button::suggested("Open worktree")
                    .on_press(action(Action::Open(path.clone())))
                    .into(),
            );
        } else if let Some(plan) = dialog
            .prepared
            .as_ref()
            .and_then(|prepared| prepared.reconciliation.as_ref())
        {
            body.push(
                widget::text::body(format!("Existing worktree: {}", plan.path().display())).into(),
            );
            if dialog.accepted && !dialog.busy() {
                body.push(design::caption("Inspect this outcome, then close and prepare checkout again before retrying.").into());
            }
            body.push(
                widget::button::suggested(if dialog.busy() {
                    "Reconciling checkout…"
                } else {
                    match plan.disposition() {
                        PrCheckoutDisposition::OpenPreservingLocal => "Keep existing local work",
                        PrCheckoutDisposition::RepairTrackingPreservingLocal => {
                            "Configure upstream and open local work"
                        }
                        PrCheckoutDisposition::AlreadyCurrent => "Verify existing worktree",
                        PrCheckoutDisposition::FastForward => "Update existing worktree",
                        PrCheckoutDisposition::RepairTracking => "Set PR upstream",
                        PrCheckoutDisposition::FastForwardAndRepair => {
                            "Update worktree and set PR upstream"
                        }
                    }
                })
                .on_press_maybe(
                    (!dialog.busy() && !dialog.accepted).then(|| action(Action::ReconcileExisting)),
                )
                .into(),
            );
        } else if let Some(request) = &dialog.request {
            body.push(
                widget::text::body(format!(
                    "Branch: {}\nDestination: {}",
                    request.branch.as_str(),
                    request.destination.display()
                ))
                .into(),
            );
            body.push(
                widget::button::suggested(if dialog.busy() {
                    "Creating…"
                } else {
                    "Create worktree"
                })
                .on_press_maybe((!dialog.busy()).then(|| action(Action::Create)))
                .into(),
            );
            body.push(
                widget::button::standard("Edit")
                    .on_press_maybe((!dialog.busy()).then(|| action(Action::Edit)))
                    .into(),
            );
        } else if !dialog.accepted {
            let mut branch = widget::text_input("New branch", &dialog.branch);
            let mut destination = widget::text_input(
                "Destination (blank uses managed worktrees)",
                &dialog.destination,
            );
            if !dialog.busy() {
                if dialog.target.pull_request.is_none() {
                    branch = branch.on_input(|v| Message::Checkout(Action::Branch(v)));
                }
                destination = destination.on_input(|v| Message::Checkout(Action::Destination(v)));
            }
            body.push(branch.into());
            body.push(destination.into());
            body.push(
                widget::button::suggested(if dialog.busy() {
                    "Resolving…"
                } else {
                    "Review checkout"
                })
                .on_press_maybe(
                    (!dialog.busy() && self.checkout_journal.is_some())
                        .then(|| action(Action::Resolve)),
                )
                .into(),
            );
        }
        if let Some(error) = &self.checkout_error {
            body.push(widget::text::body(format!("Worktree creation unavailable: {error}")).into());
        } else if self.checkout_journal.is_none() {
            body.push(widget::text::body("Loading worktree journal…").into());
        }
        if let Some(error) = &dialog.error {
            body.push(widget::text::body(error).into());
        }
        body.push(
            widget::button::standard(if dialog.busy() {
                "Cancel operation"
            } else {
                "Close"
            })
            .on_press(action(Action::Cancel))
            .into(),
        );
        Some(dialog_overlay(
            dialog_surface(widget::column::with_children(body).spacing(12), 560.0),
            action(Action::Cancel),
        ))
    }
}
