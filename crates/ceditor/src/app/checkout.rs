use super::*;
use cagents::{
    model::OperationId,
    persistence::StateStore,
    pull_request_review::CheckoutTarget,
    repository::{
        CreateWorktreeRequest, WorktreeInput, WorktreeOperationReport, WorktreeOperationState,
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
    Create,
    Finished(OperationId, Result<WorktreeOperationReport, String>),
    Cancel,
    Edit,
    Open(PathBuf),
}

pub(super) struct Dialog {
    target: CheckoutTarget,
    branch: String,
    destination: String,
    id: OperationId,
    request: Option<CreateWorktreeRequest>,
    cancellation: Option<CancellationToken>,
    error: Option<String>,
    completed: Option<PathBuf>,
    accepted: bool,
}
impl Dialog {
    pub(super) fn busy(&self) -> bool {
        self.cancellation.is_some()
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

    pub(super) fn begin_checkout(&mut self, target: CheckoutTarget) {
        if self.modal_open() {
            return;
        }
        self.checkout_dialog = Some(Dialog {
            branch: target.suggested_branch(),
            target,
            destination: String::new(),
            id: OperationId::new(),
            request: None,
            cancellation: None,
            error: None,
            completed: None,
            accepted: false,
        });
    }

    pub(super) fn checkout_action(&mut self, action: Action) -> Task<Message> {
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
                if self
                    .checkout_dialog
                    .as_ref()
                    .is_some_and(|d| d.completed.as_ref() == Some(&path))
                {
                    self.checkout_dialog = None;
                    self.selection_epoch = self.selection_epoch.wrapping_add(1);
                    return Self::open(path, self.selection_epoch);
                }
            }
            Action::Branch(value) => {
                if let Some(dialog) = self
                    .checkout_dialog
                    .as_mut()
                    .filter(|d| !d.busy() && d.request.is_none())
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
                let input = WorktreeInput {
                    repository: dialog.target.repository.clone(),
                    input: dialog.branch.clone(),
                    base: dialog.target.head.as_str().to_owned(),
                    managed_directory: self.settings.managed_worktree_directory.clone(),
                    destination: dialog.destination.clone(),
                    operation_id: id,
                };
                return cosmic::task::future(async move {
                    let result = tokio::task::spawn_blocking(move || {
                        RepositoryService::system("git")
                            .resolve_worktree_request(&input, &cancellation)
                            .map_err(|e| e.to_string())
                    })
                    .await
                    .map_err(|e| e.to_string())
                    .and_then(|r| r);
                    Message::Checkout(Action::Resolved(id, result))
                });
            }
            Action::Resolved(id, result) => {
                if let Some(dialog) = self.checkout_dialog.as_mut().filter(|d| d.id == id) {
                    let cancelled = dialog
                        .cancellation
                        .take()
                        .is_some_and(|token| token.is_cancelled());
                    if cancelled {
                        dialog.error = Some("Cancelled before creating a worktree.".into());
                    } else {
                        match result {
                            Ok(request) => dialog.request = Some(request),
                            Err(error) => dialog.error = Some(error),
                        }
                    }
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
                        Ok(report) if report.state == WorktreeOperationState::Created => {
                            dialog.completed = report.request.map(|r| r.destination);
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
                    widget::scrollable(widget::column::with_children(rows).spacing(12))
                        .height(Length::Fixed(480.0)),
                    640.0,
                ),
                Message::Checkout(Action::Cancel),
            ));
        }
        let dialog = self.checkout_dialog.as_ref()?;
        let action = |a| Message::Checkout(a);
        let mut body = vec![
            widget::text::title3("Create PR worktree").into(),
            design::caption(format!(
                "PR #{} · {}",
                dialog.target.number,
                dialog.target.head.as_str()
            ))
            .into(),
        ];
        if let Some(path) = &dialog.completed {
            body.push(widget::text::body(format!("Created {}", path.display())).into());
            body.push(
                widget::button::suggested("Open worktree")
                    .on_press(action(Action::Open(path.clone())))
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
                branch = branch.on_input(|v| Message::Checkout(Action::Branch(v)));
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
