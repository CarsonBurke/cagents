use super::*;
use cagents::{
    model::OperationId,
    persistence::StateStore,
    pull_request_review::CheckoutTarget,
    repository::{
        CreateWorktreeRequest, WorktreeInput, WorktreeOperationReport, WorktreeOperationState, RetainedWorktreeCreation, SettledCreatedWorktree,
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
    Finished(OperationId, Result<RetainedWorktreeCreation, String>),
    CreatedOpened(u64, Result<(Opened, SettledCreatedWorktree), String>),
    RetryPrivate(OperationId),
    PrivateRetried(OperationId, Result<cagents::repository::CreationPrivateRetry, String>),
    Cancel,
    Edit,
    Open(PathBuf),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum HistoryControl {
    RetainedOpen(OperationId, u64),
    RecentOpen(OperationId, u64),
    Retry(OperationId, u64),
    CloseHistory,
}

fn history_control<'a>(
    key: HistoryControl,
    content: impl Into<Element<'a, Message>>,
) -> Element<'a, Message> {
    cagents::interaction::owned_surface(content, key)
}

#[derive(Clone)]
enum Base {
    PullRequest { target: CheckoutTarget, authority: Option<WorktreeAuthority> },
    Commit {
        authority: WorktreeAuthority,
        head: cagents::repository::Oid,
    },
}
impl Base {
    fn validate(&self) -> Result<(), String> {
        match self {
            Self::Commit { authority, .. } => {
                authority.validate().map_err(|error| error.to_string())
            }
            Self::PullRequest { authority, .. } => authority.as_ref()
                .ok_or_else(|| "The original displayed worktree is unavailable. Reopen it before creating a worktree.".to_owned())?
                .validate().map_err(|error| error.to_string()),
        }
    }
    fn authority(&self) -> Option<WorktreeAuthority> {
        match self {
            Self::Commit { authority, .. } => Some(authority.clone()),
            Self::PullRequest { authority, .. } => authority.clone(),
        }
    }
    fn repository(&self) -> &Path {
        match self {
            Self::PullRequest { target, .. } => &target.repository,
            Self::Commit { authority, .. } => authority.path(),
        }
    }
    fn head(&self) -> &cagents::repository::Oid {
        match self {
            Self::PullRequest { target, .. } => &target.head,
            Self::Commit { head, .. } => head,
        }
    }
    fn branch(&self) -> String {
        match self {
            Self::PullRequest { target, .. } => target.suggested_branch(),
            Self::Commit { head, .. } => format!("review/{}", &head.as_str()[..8]),
        }
    }
}

pub(super) struct Dialog {
    target: Base,
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
        let authority = self.tabs.iter().find(|tab| tab.runtime.descriptor.worktree == target.repository)
            .and_then(|tab| tab.worktree_authority.clone());
        self.begin_checkout_base(Base::PullRequest { target, authority });
    }

    pub(super) fn begin_commit_checkout(
        &mut self,
        authority: WorktreeAuthority,
        head: cagents::repository::Oid,
    ) {
        self.begin_checkout_base(Base::Commit { authority, head });
    }

    fn begin_checkout_base(&mut self, target: Base) {
        if self.modal_open() {
            return;
        }
        self.checkout_dialog = Some(Dialog {
            branch: target.branch(),
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
                    let created = self.retained_created_checkout(id);
                    if let Some(created) = created {
                        self.checkout_history_open = false;
                        return self.open_created_checkout(created);
                    }
                    self.notice = Some("This history entry is a recorded description. Open the folder explicitly to inspect it; it cannot restore the original creation authority.".into());
                }
            }
            Action::Open(path) => {
                let created = self.checkout_dialog.as_ref()
                    .filter(|d| d.completed.as_ref() == Some(&path))
                    .and_then(|d| self.checkout_owners.get(&d.id))
                    .and_then(Option::as_ref).and_then(|retained| retained.created.clone());
                if let Some(created) = created {
                    self.checkout_dialog = None;
                    return self.open_created_checkout(created);
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
                let authority = dialog.target.clone();
                let input = WorktreeInput {
                    repository: dialog.target.repository().to_owned(),
                    input: dialog.branch.clone(),
                    base: dialog.target.head().as_str().to_owned(),
                    managed_directory: self.settings.managed_worktree_directory.clone(),
                    destination: dialog.destination.clone(),
                    operation_id: id,
                };
                return cosmic::task::future(async move {
                    let result = tokio::task::spawn_blocking(move || {
                        authority.validate()?;
                        if let Some(authority) = authority.authority() {
                            RepositoryService::system("git")
                                .with_worktree_authority(authority)
                                .and_then(|service| {
                                    service.resolve_worktree_request(&input, &cancellation)
                                })
                                .map_err(|error| error.to_string())
                        } else {
                            RepositoryService::system("git")
                                .resolve_worktree_request(&input, &cancellation)
                                .map_err(|error| error.to_string())
                        }
                    })
                    .await
                    .map_err(|e| e.to_string())
                    .and_then(|r| r);
                    Message::Checkout(Action::Resolved(id, result))
                });
            }
            Action::Resolved(id, result) => {
                if let Some(dialog) = self.checkout_dialog.as_mut().filter(|d| d.id == id) {
                    // Scoped resolution validates in its blocking worker;
                    // Create validates again before effects. UI delivery only
                    // settles this exact dialog request and cancellation.
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
                if self.checkout_owners.len() >= 32 {
                    self.notice = Some("Worktree creation ownership capacity is full. Resolve retained creations before starting another.".into());
                    return Task::none();
                }
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
                let authority = dialog.target.clone();
                self.checkout_owners.insert(id, None);
                return cosmic::task::future(async move {
                    let result = tokio::task::spawn_blocking(move || {
                        let prepared = authority.validate().and_then(|()| {
                            let original = authority.authority().ok_or_else(|| "The original displayed creation source was not retained; no worktree was created.".to_owned())?;
                            RepositoryService::system("git").with_worktree_authority(original).map_err(|error| error.to_string())
                        });
                        Ok::<_, String>(match prepared {
                            Ok(service) => cagents::pull_request_review::checkout::create_retained_with_service(&service, &journal.0, &request, &cancellation),
                            Err(error) => RetainedWorktreeCreation::not_started(&request, 1, error),
                        })
                    })
                    .await
                    .map_err(|e| e.to_string())
                    .and_then(|result| result);
                    Message::Checkout(Action::Finished(id, result))
                });
            }
            Action::Finished(id, result) => {
                // Retain BEFORE stale dialog checks and before projecting the
                // report into the capped, serializable history list.
                if let Ok(retained) = &result {
                    if retained.retention().is_some() {
                        self.checkout_owners.insert(id, Some(retained.clone()));
                    } else {
                        self.checkout_owners.remove(&id);
                    }
                    let report = &retained.report;
                    self.checkout_history.retain(|r| r.operation_id != report.operation_id);
                    self.checkout_history.push(report.clone());
                    self.trim_checkout_history();
                }
                if let Some(dialog) = self.checkout_dialog.as_mut().filter(|d| d.id == id) {
                    dialog.cancellation = None;
                    match result {
                        Ok(retained) if retained.report.state == WorktreeOperationState::Created => {
                            if let Some(created) = &retained.created {
                                dialog.completed = Some(created.record().path.clone());
                            } else {
                                dialog.error = Some(retained.authority_error.unwrap_or_else(|| "Creation completed, but its original opening authority is unavailable. Its live ownership has been retained.".into()));
                            }
                        }
                        Ok(retained) => dialog.error = Some(retained.report.detail),
                        Err(error) => {
                            // Keep the admitted negative obligation. A join or
                            // transport error cannot prove no effects occurred.
                            dialog.error = Some(format!("Checkout outcome needs reconciliation: {error}. Original ownership was not discarded."));
                        }
                    }
                    dialog.request = None;
                }
            }
            Action::CreatedOpened(epoch, result) => match result {
                Ok((opened, created)) if !self.exiting => {
                    // Directory opens always create a new tab. The worker
                    // proved the ORIGINAL destination; no path recapture here.
                    let index = self.tabs.len();
                    let task = self.add(opened, epoch == self.selection_epoch);
                    if let Some(tab) = self.tabs.get_mut(index) {
                        let operation = created.operation();
                        let generation = created.generation();
                        tab.created_worktree = Some(created);
                        // The producer only grants this SAME settled carrier
                        // after exact durable unlock and private-clear proof.
                        // Release duplicate archive ownership AFTER adoption.
                        if self.checkout_owners.get(&operation).and_then(Option::as_ref).and_then(|retained| retained.created.as_ref()).is_some_and(|original| original.generation() == generation) {
                            self.checkout_owners.remove(&operation);
                        }
                    }
                    return task;
                }
                Ok(_) => {},
                Err(error) => self.notice = Some(error),
            },
            Action::RetryPrivate(id) => {
                if self.checkout_retry_pending.contains(&id) { return Task::none(); }
                let Some(owner) = self.checkout_owners.get(&id).and_then(Option::as_ref).and_then(|retained| retained.retention()).cloned() else { return Task::none(); };
                if owner.recovery_summary().is_clear() { return Task::none(); }
                self.checkout_retry_pending.insert(id);
                return cosmic::task::future(async move {
                    let result = tokio::task::spawn_blocking(move || owner.retry_private_steps().map_err(|error| error.to_string())).await.map_err(|error| error.to_string()).and_then(|result| result);
                    Message::Checkout(Action::PrivateRetried(id, result))
                });
            }
            Action::PrivateRetried(id, result) => {
                if !self.checkout_retry_pending.remove(&id) { return Task::none(); }
                // A successful private retry never grants a new settled cap or
                // reconciles an Unknown result, and never evicts its owner.
                if self.checkout_owners.contains_key(&id) {
                    self.notice = Some(match result {
                        Ok(result) if result.pending.is_clear() => "Private creation cleanup finished. The original operation still needs reconciliation; no worktree was opened.".into(),
                        Ok(result) => format!("Private creation recovery remains pending: {}", result.failures.join("; ")),
                        Err(error) => format!("Private creation recovery was retained: {error}"),
                    });
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

    fn retained_created_checkout(&self, operation: OperationId) -> Option<SettledCreatedWorktree> {
        self.checkout_owners.get(&operation).and_then(Option::as_ref).and_then(|retained| retained.created.clone())
            .or_else(|| self.tabs.iter().find_map(|tab| tab.created_worktree.as_ref().filter(|created| created.operation() == operation).cloned()))
    }

    fn open_created_checkout(&mut self, created: SettledCreatedWorktree) -> Task<Message> {
        self.selection_epoch = self.selection_epoch.wrapping_add(1);
        let epoch = self.selection_epoch;
        let limit = self.settings.git_status_limit;
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || {
                created.validate().map_err(|error| error.to_string())?;
                let authority = created.destination().clone();
                let snapshot = read_display_snapshot(authority.path(), limit, Some(authority.clone()))?;
                created.validate().map_err(|error| error.to_string())?;
                Ok((Opened { root: authority.path().to_owned(), file: None, snapshot: Some(snapshot), authority: Some(authority) }, created))
            }).await.map_err(|error| error.to_string()).and_then(|result| result);
            Message::Checkout(Action::CreatedOpened(epoch, result))
        })
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
            if !self.checkout_owners.is_empty() {
                rows.push(widget::text::title4("Retained creation ownership").into());
                for (id, retained) in &self.checkout_owners {
                    let description = retained
                        .as_ref()
                        .and_then(|retained| retained.report.request.as_ref())
                        .map(|request| {
                            format!(
                                "{} · {}",
                                request.branch.as_str(),
                                request.destination.display()
                            )
                        })
                        .unwrap_or_else(|| format!("Creation {id}"));
                    rows.push(widget::text::body(description).into());
                    if let Some(retained) = retained {
                        rows.push(design::caption(&retained.report.detail).into());
                        if let Some(owner) = retained.retention() {
                            let pending = owner.recovery_summary();
                            if !pending.is_clear() {
                                rows.push(widget::text::body(format!("Private recovery pending: {} preparations, {} publications.", pending.pending_preparations, pending.pending_publications)).into());
                                rows.push(history_control(
                                    HistoryControl::Retry(*id, owner.generation()),
                                    widget::button::standard("Retry private recovery")
                                        .on_press_maybe(
                                            (!self.checkout_retry_pending.contains(id)).then_some(
                                                Message::Checkout(Action::RetryPrivate(*id)),
                                            ),
                                        ),
                                ));
                            } else if retained.created.is_none() {
                                rows.push(widget::text::body("Private steps are clear. The original creation still needs journal reconciliation; its ownership is retained.").into());
                            }
                        }
                        if let Some(created) = &retained.created {
                            rows.push(history_control(
                                HistoryControl::RetainedOpen(*id, created.generation()),
                                widget::button::standard("Open retained worktree")
                                    .on_press(Message::Checkout(Action::OpenRecorded(*id))),
                            ));
                        }
                    } else {
                        rows.push(widget::text::body("The original worker outcome is pending or requires reconciliation. Its admission remains retained.").into());
                    }
                }
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
                if let Some(created) = self.retained_created_checkout(report.operation_id) {
                    rows.push(history_control(
                        HistoryControl::RecentOpen(report.operation_id, created.generation()),
                        widget::button::standard("Open worktree")
                            .on_press(Message::Checkout(Action::OpenRecorded(report.operation_id))),
                    ));
                }
            }
            rows.push(history_control(
                HistoryControl::CloseHistory,
                widget::button::standard("Close").on_press(Message::Checkout(Action::Cancel)),
            ));
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
            widget::text::title3(match &dialog.target {
                Base::PullRequest { .. } => "Create PR worktree",
                Base::Commit { .. } => "Create worktree from commit",
            })
            .into(),
            design::caption(match &dialog.target {
                Base::PullRequest { target, .. } => {
                    format!("PR #{} · {}", target.number, target.head.as_str())
                }
                Base::Commit { head, .. } => format!("Base commit {}", head.as_str()),
            })
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

#[cfg(test)]
mod tests {
    use super::super::tests::{editor, git, git_fixture, open_real_git, run_tasks, sidebar_action};
    use super::*;

    fn control_pointer(
        view: &mut Element<'_, Message>,
        tree: &mut iced::advanced::widget::Tree,
        renderer: &cosmic::Renderer,
        label: &str,
        index: usize,
        event: iced::mouse::Event,
    ) -> Vec<Message> {
        control_pointer_at(view, tree, renderer, label, index, event, None).0
    }

    fn control_pointer_at(
        view: &mut Element<'_, Message>,
        tree: &mut iced::advanced::widget::Tree,
        renderer: &cosmic::Renderer,
        label: &str,
        index: usize,
        event: iced::mouse::Event,
        original_point: Option<iced::Point>,
    ) -> (Vec<Message>, iced::Point) {
        use iced::advanced::{Layout, Shell, layout, mouse};
        let viewport = iced::Rectangle::with_size(iced::Size::new(1000.0, 800.0));
        let node = view.as_widget_mut().layout(
            tree,
            renderer,
            &layout::Limits::new(iced::Size::ZERO, viewport.size()),
        );
        let accessible =
            view.as_widget()
                .a11y_nodes(Layout::new(&node), tree, mouse::Cursor::Unavailable);
        let button = accessible
            .root()
            .iter()
            .chain(accessible.children())
            .filter(|entry| entry.node().label() == Some(label))
            .nth(index)
            .expect("the selected control is present in the actual widget tree");
        assert!(!button.node().is_disabled());
        let bounds = button.node().bounds().unwrap();
        let point = iced::Point::new(
            ((bounds.x0 + bounds.x1) / 2.0) as f32,
            ((bounds.y0 + bounds.y1) / 2.0) as f32,
        );
        assert!(
            viewport.contains(point),
            "the control must be in the viewport"
        );
        let point = original_point.map_or(point, |original| {
            assert_eq!(
                point, original,
                "the replacement occupies the original press coordinate"
            );
            original
        });
        let mut messages = Vec::new();
        view.as_widget_mut().update(
            tree,
            &iced::Event::Mouse(event),
            Layout::new(&node),
            mouse::Cursor::Available(point),
            renderer,
            &mut iced::advanced::clipboard::Null,
            &mut Shell::new(&mut messages),
            &viewport,
        );
        (messages, point)
    }

    fn check_history_press_ownership(
        app: &mut Editor,
        first: &WorktreeOperationReport,
        second: &WorktreeOperationReport,
    ) {
        use iced::advanced::{renderer::Headless, widget::Tree};
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let renderer = runtime
            .block_on(<cosmic::Renderer as Headless>::new(
                iced::Font::DEFAULT,
                iced::Pixels::from(16),
                Some("tiny-skia"),
            ))
            .unwrap();
        let press = iced::mouse::Event::ButtonPressed(iced::mouse::Button::Left);
        let release = iced::mouse::Event::ButtonReleased(iced::mouse::Button::Left);
        assert!(
            app.checkout_owners.is_empty(),
            "both original carriers were adopted"
        );
        assert_ne!(first.operation_id, second.operation_id);
        let saved_history = std::mem::replace(
            &mut app.checkout_history,
            vec![first.clone(), second.clone()],
        );
        // This is a controlled chronological projection change, not an assertion
        // that a provider or a private-recovery worker was exercised by this frame.
        app.checkout_history[0].created_at_unix_ms = 2;
        app.checkout_history[1].created_at_unix_ms = 1;
        app.trim_checkout_history();
        drop(app.update(Message::Checkout(Action::History)));
        assert!(app.checkout_history_open);
        let mut before = app.checkout_view().unwrap();
        let mut tree = Tree::new(before.as_widget());
        let (messages, original_point) = control_pointer_at(
            &mut before,
            &mut tree,
            &renderer,
            "Open worktree",
            0,
            press,
            None,
        );
        assert!(messages.is_empty());
        drop(before);
        app.checkout_history[0].created_at_unix_ms = 0;
        app.trim_checkout_history();
        assert_eq!(app.checkout_history[0].operation_id, second.operation_id);
        let mut after = app.checkout_view().unwrap();
        tree.diff(after.as_widget_mut());
        assert!(
            control_pointer_at(
                &mut after,
                &mut tree,
                &renderer,
                "Open worktree",
                0,
                release,
                Some(original_point),
            )
            .0
            .is_empty(),
            "an A press cannot become B's Open action after a retained-tree rebuild"
        );
        assert!(
            control_pointer(&mut after, &mut tree, &renderer, "Open worktree", 0, press).is_empty()
        );
        let messages = control_pointer(
            &mut after,
            &mut tree,
            &renderer,
            "Open worktree",
            0,
            release,
        );
        assert!(
            matches!(messages.as_slice(), [Message::Checkout(Action::OpenRecorded(id))] if *id == second.operation_id),
            "a fresh B click must still select B's original carrier"
        );
        drop(after);
        let mut unchanged = app.checkout_view().unwrap();
        tree.diff(unchanged.as_widget_mut());
        assert!(
            control_pointer(
                &mut unchanged,
                &mut tree,
                &renderer,
                "Open worktree",
                1,
                press
            )
            .is_empty()
        );
        drop(unchanged);
        let mut unchanged = app.checkout_view().unwrap();
        tree.diff(unchanged.as_widget_mut());
        let messages = control_pointer(
            &mut unchanged,
            &mut tree,
            &renderer,
            "Open worktree",
            1,
            release,
        );
        assert!(
            matches!(messages.as_slice(), [Message::Checkout(Action::OpenRecorded(id))] if *id == first.operation_id),
            "the same A control must keep a valid press across an unchanged rebuild"
        );
        drop(unchanged);
        // Exercise role/generation replacement through the production wrapper.
        // These are widget ownership cases, not private-cleanup end-to-end tests.
        let retry = HistoryControl::Retry(first.operation_id, 1);
        for (original, replacement) in [
            (retry, HistoryControl::Retry(second.operation_id, 1)),
            (retry, HistoryControl::Retry(first.operation_id, 2)),
            (retry, HistoryControl::RetainedOpen(first.operation_id, 1)),
            (retry, HistoryControl::RecentOpen(first.operation_id, 1)),
            (retry, HistoryControl::CloseHistory),
            (
                HistoryControl::RetainedOpen(first.operation_id, 1),
                HistoryControl::RetainedOpen(second.operation_id, 1),
            ),
            (
                HistoryControl::RetainedOpen(first.operation_id, 1),
                HistoryControl::RetainedOpen(first.operation_id, 2),
            ),
            (
                HistoryControl::RecentOpen(first.operation_id, 1),
                HistoryControl::RecentOpen(first.operation_id, 2),
            ),
        ] {
            let control = |key| {
                history_control(
                    key,
                    widget::button::standard("Action").on_press(Message::Checkout(Action::Cancel)),
                )
            };
            let mut before = control(original);
            let mut tree = Tree::new(before.as_widget());
            assert!(
                control_pointer(&mut before, &mut tree, &renderer, "Action", 0, press).is_empty()
            );
            drop(before);
            let mut after = control(replacement);
            tree.diff(after.as_widget_mut());
            assert!(
                control_pointer(&mut after, &mut tree, &renderer, "Action", 0, release).is_empty()
            );
            assert!(
                control_pointer(&mut after, &mut tree, &renderer, "Action", 0, press).is_empty()
            );
            assert!(
                matches!(
                    control_pointer(&mut after, &mut tree, &renderer, "Action", 0, release)
                        .as_slice(),
                    [Message::Checkout(Action::Cancel)]
                ),
                "a fresh replacement action remains usable"
            );
        }
        app.checkout_history = saved_history;
        drop(app.update(Message::Checkout(Action::Cancel)));
        assert!(!app.checkout_history_open);
    }

    fn click_retained_checkout(app: &Editor, label: &str, expected_description: &str) -> Message {
        use iced::advanced::{Layout, Shell, layout, mouse, renderer::Headless, widget::Tree};
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let renderer = runtime.block_on(<cosmic::Renderer as Headless>::new(iced::Font::DEFAULT, iced::Pixels::from(16), Some("tiny-skia"))).unwrap();
        let mut view = app.checkout_view().expect("actual checkout surface");
        let mut tree = Tree::new(view.as_widget());
        let viewport = iced::Rectangle::with_size(iced::Size::new(1000.0, 800.0));
        let node = view.as_widget_mut().layout(&mut tree, &renderer, &layout::Limits::new(iced::Size::ZERO, viewport.size()));
        let point = {
            let accessible = view.as_widget().a11y_nodes(Layout::new(&node), &tree, mouse::Cursor::Unavailable);
            assert!(
                accessible.root().iter().chain(accessible.children()).any(|entry| entry.node().label() == Some(expected_description)),
                "the evicted retained creation must still identify its branch and destination"
            );
            let button = accessible.root().iter().chain(accessible.children()).find(|entry| entry.node().label() == Some(label)).expect("retained owner control stays mounted after history eviction");
            assert!(!button.node().is_disabled()); let bounds = button.node().bounds().unwrap();
            iced::Point::new(((bounds.x0 + bounds.x1) / 2.0) as f32, ((bounds.y0 + bounds.y1) / 2.0) as f32)
        };
        let mut messages = Vec::new();
        for event in [iced::mouse::Event::ButtonPressed(iced::mouse::Button::Left), iced::mouse::Event::ButtonReleased(iced::mouse::Button::Left)] {
            view.as_widget_mut().update(&mut tree, &iced::Event::Mouse(event), Layout::new(&node), mouse::Cursor::Available(point), &renderer, &mut iced::advanced::clipboard::Null, &mut Shell::new(&mut messages), &viewport);
        }
        let message = messages.pop().expect("actual button emits its owner action"); assert!(messages.is_empty()); message
    }

    #[cfg(target_os = "linux")]
    fn check_pre_admission_cancellation_releases_creation_capacity() {
        let repository = git_fixture();
        let data = tempfile::tempdir().unwrap();
        let mut app = editor();
        app.settings_path = data.path().join("settings.toml");
        let task = app.load_checkout_journal();
        run_tasks(&mut app, task);
        let card = open_real_git(&mut app, repository.path(), None);
        let authority = app.tab(card).unwrap().worktree_authority.clone().unwrap();
        let head = cagents::repository::Oid::parse(&git(repository.path(), &["rev-parse", "HEAD"]))
            .unwrap();
        for index in 0..33 {
            app.begin_commit_checkout(authority.clone(), head.clone());
            let branch = format!("cancelled-{index}");
            drop(app.update(Message::Checkout(Action::Branch(branch))));
            let destination = data.path().join(format!("cancelled-{index}"));
            drop(app.update(Message::Checkout(Action::Destination(
                destination.to_string_lossy().into_owned(),
            ))));
            let task = app.update(Message::Checkout(Action::Resolve));
            run_tasks(&mut app, task);
            let dialog = app.checkout_dialog.as_ref().unwrap();
            assert!(dialog.request.is_some(), "{:?}", dialog.error);
            let id = dialog.id;
            let task = app.update(Message::Checkout(Action::Create));
            assert!(app.checkout_dialog.as_ref().unwrap().accepted);
            assert!(app.checkout_owners.get(&id).is_some_and(Option::is_none));
            drop(app.update(Message::CloseWindow));
            assert!(!app.exiting, "pending creation still owns close custody");
            // The returned future is cold: cancel before the worker starts,
            // then exercise the real captured producer and Finished reducer.
            drop(app.update(Message::Checkout(Action::Cancel)));
            run_tasks(&mut app, task);
            let report = app
                .checkout_history
                .iter()
                .find(|report| report.operation_id == id)
                .unwrap();
            assert_eq!(report.state, WorktreeOperationState::NotCreated);
            assert!(matches!(
                report.outcome.as_ref(),
                Some(cagents::repository::WorktreeCreationOutcome::NotCreated { .. })
            ));
            assert!(
                app.checkout_owners.is_empty(),
                "clean cancellation must retire its admitted placeholder"
            );
            assert!(!app.checkout_dialog.as_ref().unwrap().busy());
            assert!(!destination.exists());
            assert!(
                app.checkout_journal
                    .as_ref()
                    .unwrap()
                    .0
                    .journaled_worktree_operations()
                    .unwrap()
                    .is_empty()
            );
            drop(app.update(Message::Checkout(Action::Cancel)));
        }
        assert_eq!(app.checkout_history.len(), 20);
        assert_eq!(
            git(repository.path(), &["branch", "--list", "cancelled-*"]),
            ""
        );
        drop(app.update(Message::CloseWindow));
        assert!(
            app.exiting,
            "more than 32 clean cancellations must not exhaust close or creation capacity"
        );

        // An uncertain worker completion retains its pending placeholder.
        let mut uncertain = editor();
        let id = OperationId::new();
        uncertain.checkout_owners.insert(id, None);
        drop(uncertain.update(Message::Checkout(Action::Finished(
            id,
            Err("worker result unavailable".into()),
        ))));
        assert!(uncertain.checkout_owners.get(&id).is_some_and(Option::is_none));
        drop(uncertain.update(Message::CloseWindow));
        assert!(!uncertain.exiting);
    }

    #[test]
    fn actual_created_receipt_survives_dialog_loss_and_refuses_late_replacement_open() {
        use cosmic::iced::futures::StreamExt;
        use cagents::process::{CommandSpec, ProcessExecutor, SystemProcessExecutor};
        const MARKER: &str = "CAGENTS_CEDITOR_RETAINED_CREATION_FIXTURE";
        const FULL: &str = concat!(module_path!(), "::actual_created_receipt_survives_dialog_loss_and_refuses_late_replacement_open");
        let name = FULL.strip_prefix(concat!(env!("CARGO_CRATE_NAME"), "::")).unwrap_or(FULL);
        if std::env::var_os(MARKER).as_deref() != Some(std::ffi::OsStr::new(name)) {
            let mut command = CommandSpec::new(std::env::current_exe().unwrap()).args(["--exact", name, "--nocapture"]);
            for (key, _) in std::env::vars_os().filter(|(key, _)| key.as_encoded_bytes().starts_with(b"GIT_")) { command = command.env_remove(key); }
            command = command.env(MARKER, name).env("GIT_CONFIG_NOSYSTEM", "1").env("GIT_CONFIG_SYSTEM", "/dev/null").env("GIT_CONFIG_GLOBAL", "/dev/null");
            command.timeout = Some(Duration::from_secs(60));
            let output = SystemProcessExecutor.execute(&command, &CancellationToken::new()).unwrap();
            {
                use std::io::Write as _;
                let mut stdout = std::io::stdout().lock();
                writeln!(
                    stdout,
                    "BEGIN captured child stdout: parent={name} child={name}"
                )
                .unwrap();
                stdout.write_all(&output.stdout).unwrap();
                writeln!(
                    stdout,
                    "\nEND captured child stdout: parent={name} child={name}"
                )
                .unwrap();
            }
            {
                use std::io::Write as _;
                let mut stderr = std::io::stderr().lock();
                writeln!(
                    stderr,
                    "BEGIN captured child stderr: parent={name} child={name}"
                )
                .unwrap();
                stderr.write_all(&output.stderr).unwrap();
                writeln!(
                    stderr,
                    "\nEND captured child stderr: parent={name} child={name}"
                )
                .unwrap();
            }
            assert!(output.success, "{}", String::from_utf8_lossy(&output.stderr));
            assert!(String::from_utf8_lossy(&output.stdout).contains("test result: ok. 1 passed; 0 failed"), "exact child must actually run once: {}", String::from_utf8_lossy(&output.stdout));
            let stdout = String::from_utf8_lossy(&output.stdout);
            let expected = format!("test {name} ... ok");
            assert!(
                stdout.lines().any(|line| line == expected),
                "the exact selected child must pass: {stdout}"
            );
            assert!(
                stdout
                    .lines()
                    .any(|line| line.starts_with("test result: ok. 1 passed; 0 failed; 0 ignored;")),
                "the exact child must report 1 passed, 0 failed and 0 ignored: {stdout}"
            );
            return;
        }
        #[cfg(target_os = "linux")]
        check_pre_admission_cancellation_releases_creation_capacity();
        let repository = git_fixture(); let data = tempfile::tempdir().unwrap();
        let destination = data.path().join("created");
        let mut app = editor(); app.settings_path = data.path().join("settings.toml");
        let task = app.load_checkout_journal(); run_tasks(&mut app, task);
        let card = open_real_git(&mut app, repository.path(), None);
        let authority = app.tab(card).unwrap().worktree_authority.clone().unwrap();
        let head = cagents::repository::Oid::parse(&git(repository.path(), &["rev-parse", "HEAD"])).unwrap();
        app.begin_commit_checkout(authority.clone(), head.clone());
        drop(app.update(Message::Checkout(Action::Branch("retained-child".into()))));
        drop(app.update(Message::Checkout(Action::Destination(destination.to_string_lossy().into_owned()))));
        let task = app.update(Message::Checkout(Action::Resolve)); run_tasks(&mut app, task);
        let id = app.checkout_dialog.as_ref().unwrap().id;
        let task = app.update(Message::Checkout(Action::Create));
        assert!(app.checkout_owners.contains_key(&id), "reserve ownership before polling any effectful worker");
        let receipt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async move {
            let mut stream = iced::runtime::task::into_stream(task).expect("real creation worker");
            loop {
                let action = stream.next().await.expect("creation must deliver its ownership");
                if let iced::runtime::Action::Output(cosmic::Action::App(message @ Message::Checkout(Action::Finished(..)))) = action { break message; }
            }
        });
        app.checkout_dialog = None; app.tabs.clear(); app.active = None;
        drop(app.update(receipt));
        let retained = app.checkout_owners.get(&id).and_then(Option::as_ref).expect("late outcome retains original session before checking dialog");
        assert!(retained.retention().is_some()); assert!(retained.created.is_some(), "{:?}", retained.authority_error);
        let original_report = app.checkout_owners[&id].as_ref().unwrap().report.clone();
        for index in 0..20 {
            let mut request = original_report.request.as_ref().unwrap().clone(); request.operation_id = OperationId::new();
            request.branch = cagents::repository::ValidatedBranchName::parse(format!("newer-fixture-{index}")).unwrap();
            request.destination = data.path().join(format!("newer-fixture-{index}"));
            let mut newer = RetainedWorktreeCreation::not_started(&request, 1, "actual typed preflight refusal fixture".into());
            newer.report.created_at_unix_ms = original_report.created_at_unix_ms + index + 1;
            drop(app.update(Message::Checkout(Action::Finished(request.operation_id, Ok(newer)))));
        }
        assert_eq!(app.checkout_history.len(), 20);
        assert!(!app.checkout_history.iter().any(|report| report.operation_id == id), "serialized history evicts the old description");
        assert!(app.checkout_history.iter().filter_map(|report| report.request.as_ref()).all(|request| {
            request.branch.as_str() != "retained-child" && request.destination != destination
        }), "newer history cannot satisfy the evicted original's accessible description");
        assert!(app.checkout_owners.get(&id).and_then(Option::as_ref).is_some(), "live ownership is independent");
        drop(app.update(Message::CloseWindow));
        assert!(!app.exiting, "unresolved transfer/lifecycle proof cannot disappear on exit");
        let created = app.checkout_owners[&id].as_ref().unwrap().created.clone().unwrap();
        drop(app.update(Message::Checkout(Action::History)));
        let emitted = click_retained_checkout(
            &app,
            "Open retained worktree",
            &format!("retained-child · {}", destination.display()),
        );
        assert!(matches!(emitted, Message::Checkout(Action::OpenRecorded(operation)) if operation == id), "actual mounted live-owner row names the evicted original");
        let task = app.update(emitted); run_tasks(&mut app, task);
        assert!(app.tabs.iter().any(|tab| tab.created_worktree.as_ref().is_some_and(|cap| cap.operation() == id)));
        assert!(app.checkout_owners.is_empty(), "the SAME settled carrier is now owned by the mounted tab");
        // Produce a distinct real B carrier before the cached native ownership
        // checks. Reusing/relabeling A would conceal a wrong-target selection.
        app.begin_commit_checkout(authority, head);
        drop(app.update(Message::Checkout(Action::Branch("retained-second".into()))));
        drop(
            app.update(Message::Checkout(Action::Destination(
                data.path()
                    .join("second-created")
                    .to_string_lossy()
                    .into_owned(),
            ))),
        );
        let task = app.update(Message::Checkout(Action::Resolve));
        run_tasks(&mut app, task);
        let second_id = app.checkout_dialog.as_ref().unwrap().id;
        let task = app.update(Message::Checkout(Action::Create));
        run_tasks(&mut app, task);
        let second = app.checkout_owners[&second_id].as_ref().unwrap();
        assert!(second.created.is_some(), "{:?}", second.authority_error);
        let second_report = second.report.clone();
        let second_path = second.created.as_ref().unwrap().record().path.clone();
        let task = app.update(Message::Checkout(Action::Open(second_path)));
        run_tasks(&mut app, task);
        assert!(app.tabs.iter().any(|tab| {
            tab.created_worktree
                .as_ref()
                .is_some_and(|cap| cap.operation() == second_id)
        }));
        check_history_press_ownership(&mut app, &original_report, &second_report);
        app.tabs.clear(); app.active = None;
        std::fs::rename(&destination, data.path().join("parked")).unwrap();
        std::fs::create_dir(&destination).unwrap(); std::fs::write(destination.join("foreign"), "untouched").unwrap();
        let task = app.open_created_checkout(created); run_tasks(&mut app, task);
        assert!(app.tabs.is_empty(), "the original created scope cannot recapture its replacement path");
        assert_eq!(std::fs::read_to_string(destination.join("foreign")).unwrap(), "untouched");
        assert!(app.checkout_owners.is_empty(), "clean ownership was transferred to the original mounted carrier, not reconstructed from history");
        drop(app.update(Message::CloseWindow)); assert!(app.exiting, "clean successful adoption does not permanently starve ordinary exit");
    }

    #[test]
    fn standalone_history_creates_real_worktree_from_captured_commit_dialog() {
        let directory = git_fixture();
        let root = directory.path();
        let private_state = tempfile::tempdir().unwrap();
        let destination = private_state.path().join("commit-checkout");
        let mut app = editor();
        app.settings_path = private_state.path().join("settings.toml");
        app.settings.managed_worktree_directory = private_state
            .path()
            .join("managed")
            .to_string_lossy()
            .into_owned();
        let journal = app.load_checkout_journal();
        run_tasks(&mut app, journal);
        assert!(app.checkout_journal.is_some(), "{:?}", app.checkout_error);
        let card = open_real_git(&mut app, root, None);
        let key = app
            .tab(card)
            .unwrap()
            .navigation
            .active_worktree()
            .unwrap()
            .clone();
        sidebar_action(
            &mut app,
            SidebarAction::SetChangesTab {
                key: key.clone(),
                tab: sidebar::ChangesTab::History,
            },
        );
        let row = app
            .tab(card)
            .unwrap()
            .sidebar
            .source_control
            .as_ref()
            .unwrap()
            .history[0]
            .clone();
        sidebar_action(
            &mut app,
            SidebarAction::HistoryCommand {
                key,
                oid: row.oid.clone(),
                command: sidebar::HistoryCommand::NewWorktree,
                mainline: row.mainline,
                message: row.message,
            },
        );
        let dialog = app.checkout_dialog.as_ref().expect("actual dialog mounted");
        assert_eq!(dialog.target.repository(), root);
        assert_eq!(dialog.target.head(), &row.oid);
        assert!(matches!(dialog.target, Base::Commit { .. }));
        assert!(app.checkout_view().is_some());
        drop(app.update(Message::Checkout(Action::Branch("history-checkout".into()))));
        drop(app.update(Message::Checkout(Action::Destination(
            destination.to_string_lossy().into_owned(),
        ))));
        let task = app.update(Message::Checkout(Action::Resolve));
        run_tasks(&mut app, task);
        assert!(
            app.checkout_dialog.as_ref().unwrap().request.is_some(),
            "{:?}",
            app.checkout_dialog.as_ref().unwrap().error
        );
        let task = app.update(Message::Checkout(Action::Create));
        run_tasks(&mut app, task);
        assert_eq!(
            app.checkout_dialog.as_ref().unwrap().completed.as_deref(),
            Some(destination.as_path())
        );
        assert_eq!(git(&destination, &["rev-parse", "HEAD"]), row.oid.as_str());
        assert_eq!(
            git(&destination, &["branch", "--show-current"]),
            "history-checkout"
        );
        assert!(
            app.checkout_history
                .iter()
                .any(|report| report.state == WorktreeOperationState::Created)
        );
    }
}
