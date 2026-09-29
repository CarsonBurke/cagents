use super::*;
use crate::{
    model::OperationId,
    repository::{
        WorktreeOperationReport, WorktreeOperationState, WorktreeProgressEvent,
        WorktreeProgressStage,
    },
    sidebar::{WorktreeOperationPhase, WorktreeOperationRow},
};

pub(super) struct ActiveWorktreeOperation {
    project: ProjectId,
    generation: u64,
    request: Option<CreateWorktreeRequest>,
    token: Option<CancellationToken>,
    receiver: Option<async_channel::Receiver<WorktreeProgressEvent>>,
    stage: Option<WorktreeProgressStage>,
    started: Instant,
    completed_elapsed: Option<Duration>,
    restored: bool,
    termination_requested: bool,
    finished: bool,
    detail: String,
    created_worktree: Option<(PathBuf, i64)>,
    /// The name the worktree's sidebar row will have, known from the request.
    label: String,
    /// The journal's verdict once the operation has finished.
    state: Option<WorktreeOperationState>,
    /// What the user typed to start it, when this session started it: a
    /// failure can then be reopened in the dialog instead of retyped.
    draft: Option<WorktreeDraft>,
}

impl ActiveWorktreeOperation {
    pub(super) fn project(&self) -> ProjectId {
        self.project
    }

    pub(super) fn detail(&self) -> &str {
        &self.detail
    }

    /// The input of a creation that ended without a worktree, which the
    /// dialog can take back to edit or run again.
    pub(super) fn reopenable_draft(&self) -> Option<&WorktreeDraft> {
        self.draft
            .as_ref()
            .filter(|_| self.finished && self.created_worktree.is_none())
            .filter(|_| self.state != Some(WorktreeOperationState::Created))
    }

    /// How the sidebar shows this operation, if at all.
    ///
    /// A creation that succeeded is shown by the worktree row itself as soon
    /// as the checkout lists it, and one restored from the journal after a
    /// restart has either been listed long ago or been deleted since: neither
    /// needs a row. The same goes for a restored creation that was cleanly
    /// rolled back. Everything else - a creation still running, one that
    /// failed in this session, or a journal entry that could not be settled
    /// - is shown until it resolves or is dismissed.
    fn row(
        &self,
        worktrees: &[crate::sidebar::SidebarWorktree],
    ) -> Option<(WorktreeOperationPhase, String)> {
        if !self.finished {
            let detail = if self.termination_requested {
                "Cancelling".to_owned()
            } else {
                format!("{} ({}s)", self.detail, self.started.elapsed().as_secs())
            };
            return Some((WorktreeOperationPhase::Working, detail));
        }
        match self.state {
            Some(WorktreeOperationState::Created) => {
                let listed = self.created_worktree.as_ref().is_some_and(|(path, _)| {
                    worktrees.iter().any(|worktree| worktree.path == *path)
                });
                (!self.restored && !listed).then(|| {
                    (
                        WorktreeOperationPhase::Working,
                        "Created; waiting for the checkout to list it".to_owned(),
                    )
                })
            }
            Some(WorktreeOperationState::NotCreated) if self.restored => None,
            _ => Some((WorktreeOperationPhase::Failed, self.detail.clone())),
        }
    }
}

/// The sidebar name of the worktree a request creates: what its row will read
/// once the checkout lists it, so the pending row turns into it in place.
fn worktree_operation_label(request: Option<&CreateWorktreeRequest>) -> String {
    request.map_or_else(
        || "Unknown worktree".to_owned(),
        |request| {
            crate::sidebar::worktree_display_name(
                &request.destination,
                Some(request.branch.as_str()),
                false,
            )
        },
    )
}

#[derive(Clone)]
struct ProgressSource(OperationId, async_channel::Receiver<WorktreeProgressEvent>);
impl std::hash::Hash for ProgressSource {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.0.hash(state);
    }
}
fn stream(
    source: &ProgressSource,
) -> Pin<Box<dyn cosmic::iced::futures::Stream<Item = WorktreeProgressEvent> + Send>> {
    Box::pin(cosmic::iced::futures::stream::unfold(
        source.1.clone(),
        |receiver| async move { receiver.recv().await.ok().map(|event| (event, receiver)) },
    ))
}
fn elapsed_stream(_: &u8) -> Pin<Box<dyn cosmic::iced::futures::Stream<Item = ()> + Send>> {
    Box::pin(cosmic::iced::futures::stream::unfold((), |_| async {
        tokio::time::sleep(Duration::from_secs(1)).await;
        Some(((), ()))
    }))
}
fn stage_label(stage: WorktreeProgressStage) -> &'static str {
    match stage {
        WorktreeProgressStage::Validating => "Validating the request",
        WorktreeProgressStage::ResolvingBase => "Resolving the base revision",
        WorktreeProgressStage::RunningGit => "Running git worktree add",
        WorktreeProgressStage::Reconciling => "Checking the result",
        WorktreeProgressStage::Finished => "Recording the outcome",
    }
}
fn stage_order(stage: WorktreeProgressStage) -> u8 {
    match stage {
        WorktreeProgressStage::Validating => 0,
        WorktreeProgressStage::ResolvingBase => 1,
        WorktreeProgressStage::RunningGit => 2,
        WorktreeProgressStage::Reconciling => 3,
        WorktreeProgressStage::Finished => 4,
    }
}
impl Cagents {
    pub(super) fn has_active_worktree_operations(&self) -> bool {
        self.worktree_operations
            .values()
            .any(|operation| !operation.finished)
    }

    pub(super) fn start_worktree_operation(
        &mut self,
        project: ProjectId,
        request: CreateWorktreeRequest,
        draft: Option<WorktreeDraft>,
    ) -> Task<Message> {
        let Some(session) = self.session.as_ref() else {
            self.status = Some(TransientNotice::error(
                "Worktree creation requires an available durable journal. No Git mutation was started.",
            ));
            return Task::none();
        };
        let store = session.operation_store();
        let token = CancellationToken::new();
        let (sender, receiver) = async_channel::bounded(8);
        let id = request.operation_id;
        let generation = 1;
        self.worktree_operations.insert(
            id,
            ActiveWorktreeOperation {
                project,
                generation,
                label: worktree_operation_label(Some(&request)),
                request: Some(request.clone()),
                token: Some(token.clone()),
                receiver: Some(receiver),
                stage: None,
                started: Instant::now(),
                termination_requested: false,
                finished: false,
                detail: "Starting".into(),
                completed_elapsed: None,
                restored: false,
                created_worktree: None,
                state: None,
                draft,
            },
        );
        self.sync_worktree_operation_rows();
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || {
                RepositoryService::system("git")
                    .create_worktree_journaled(&store, &request, generation, &token, &sender)
            })
            .await
            .map_err(|error| {
                format!("Worker unavailable; journal reconciliation required: {error}")
            });
            Message::WorktreeOperationFinished {
                operation: id,
                generation,
                result,
            }
        })
    }
    pub(super) fn worktree_progress(&mut self, event: WorktreeProgressEvent) -> Task<Message> {
        let Some(operation) = self.worktree_operations.get_mut(&event.operation_id) else {
            return Task::none();
        };
        if operation.generation != event.generation
            || operation.finished
            || operation
                .stage
                .is_some_and(|old| stage_order(old) >= stage_order(event.stage))
        {
            return Task::none();
        }
        operation.stage = Some(event.stage);
        operation.detail = stage_label(event.stage).into();
        self.sync_worktree_operation_rows();
        Task::none()
    }
    /// Drops the row of a creation that has finished without a worktree. A
    /// running creation keeps its row: cancel is the way to stop it, and its
    /// outcome is what says whether anything was left behind.
    pub(super) fn dismiss_worktree_operation(&mut self, id: OperationId) -> Task<Message> {
        if self
            .worktree_operations
            .get(&id)
            .is_some_and(|operation| operation.finished && operation.created_worktree.is_none())
        {
            self.worktree_operations.remove(&id);
            self.sync_worktree_operation_rows();
        }
        Task::none()
    }
    /// Forgets the finished creation of a worktree the user has since deleted.
    /// Its row waits for the checkout to list the worktree, which it never
    /// will again.
    pub(super) fn forget_worktree_creation(&mut self, path: &Path) {
        self.worktree_operations.retain(|_, operation| {
            !(operation.finished
                && operation
                    .created_worktree
                    .as_ref()
                    .is_some_and(|(created, _)| created == path))
        });
        self.sync_worktree_operation_rows();
    }

    pub(super) fn cancel_worktree_operation(&mut self, id: OperationId) -> Task<Message> {
        let Some(operation) = self.worktree_operations.get_mut(&id) else {
            return Task::none();
        };
        if operation.finished || operation.termination_requested {
            return Task::none();
        }
        if let Some(token) = operation.token.as_ref() {
            token.cancel();
            operation.termination_requested = true;
        }
        self.sync_worktree_operation_rows();
        Task::none()
    }
    pub(super) fn finish_worktree_operation(
        &mut self,
        id: OperationId,
        generation: u64,
        result: Result<WorktreeOperationReport, String>,
    ) -> Task<Message> {
        let Some(operation) = self.worktree_operations.get_mut(&id) else {
            return Task::none();
        };
        if operation.generation != generation || operation.finished {
            return Task::none();
        }
        if result
            .as_ref()
            .is_ok_and(|report| report.operation_id != id || report.generation != generation)
        {
            return Task::none();
        }
        operation.finished = true;
        operation.completed_elapsed = Some(operation.started.elapsed());
        operation.token = None;
        operation.receiver = None;
        let project_id = operation.project;
        let repository = operation
            .request
            .as_ref()
            .map(|request| request.repository.clone());
        let outcome = match result {
            Ok(report) => {
                operation.created_worktree = match &report.outcome {
                    Some(WorktreeCreationOutcome::Created { worktree, .. }) => {
                        Some((worktree.path.clone(), report.created_at_unix_ms))
                    }
                    _ => None,
                };
                operation.state = Some(report.state);
                operation.detail = report.detail;
                report.outcome
            }
            Err(error) => {
                operation.state = Some(WorktreeOperationState::OutcomeUnknown);
                operation.detail = error;
                None
            }
        };
        self.sync_worktree_operation_rows();
        if let (Some(repository), Some(outcome @ WorktreeCreationOutcome::Created { .. })) =
            (repository, outcome)
        {
            return cosmic::task::message(Message::WorktreeCreated {
                project_id,
                repository,
                result: Ok(outcome),
            });
        }
        Task::none()
    }
    pub(super) fn rehydrate_worktree_operations(&self) -> Task<Message> {
        let Some(session) = self.session.as_ref() else {
            return Task::none();
        };
        let store = session.operation_store();
        cosmic::task::future(async move {
            Message::WorktreeOperationsRehydrated(
                tokio::task::spawn_blocking(move || {
                    RepositoryService::system("git").rehydrate_worktree_operations(&store)
                })
                .await
                .map_err(|error| error.to_string())
                .and_then(|result| result.map_err(|error| error.to_string())),
            )
        })
    }
    pub(super) fn restore_worktree_operations(
        &mut self,
        result: Result<Vec<WorktreeOperationReport>, String>,
    ) -> Task<Message> {
        match result {
            Ok(reports) => {
                for report in reports {
                    if self.worktree_operations.contains_key(&report.operation_id) {
                        continue;
                    }
                    let project = report
                        .request
                        .as_ref()
                        .and_then(|request| {
                            self.sidebar_model
                                .projects
                                .iter()
                                .find(|project| project.repository == request.repository)
                        })
                        .map(|project| project.id);
                    let Some(project) = project else {
                        self.status = Some(TransientNotice::warning(format!(
                            "Journal operation {} remains inspectable in the journal: {:?}: {}",
                            report.operation_id, report.state, report.detail
                        )));
                        continue;
                    };
                    self.worktree_operations.insert(
                        report.operation_id,
                        ActiveWorktreeOperation {
                            project,
                            created_worktree: match &report.outcome {
                                Some(WorktreeCreationOutcome::Created { worktree, .. }) => {
                                    Some((worktree.path.clone(), report.created_at_unix_ms))
                                }
                                _ => None,
                            },
                            generation: report.generation,
                            label: worktree_operation_label(report.request.as_ref()),
                            request: report.request,
                            token: None,
                            receiver: None,
                            stage: None,
                            started: Instant::now(),
                            termination_requested: false,
                            finished: true,
                            completed_elapsed: None,
                            restored: true,
                            detail: report.detail,
                            state: Some(report.state),
                            draft: None,
                        },
                    );
                }
            }
            Err(error) => {
                self.status = Some(TransientNotice::error(format!(
                    "Worktree journal recovery failed: {error}"
                )))
            }
        }
        self.sync_worktree_operation_rows();
        Task::none()
    }
    pub(super) fn worktree_progress_subscriptions(&self) -> Vec<Subscription<Message>> {
        let mut subscriptions: Vec<_> = self
            .worktree_operations
            .iter()
            .filter_map(|(&id, operation)| {
                operation.receiver.as_ref().map(|receiver| {
                    Subscription::run_with(ProgressSource(id, receiver.clone()), stream)
                        .map(Message::WorktreeProgress)
                })
            })
            .collect();
        if self
            .worktree_operations
            .values()
            .any(|operation| !operation.finished)
        {
            subscriptions.push(
                Subscription::run_with(0_u8, elapsed_stream).map(|()| Message::WorktreeElapsed),
            );
        }
        subscriptions
    }
    pub(super) fn sync_worktree_operation_rows(&mut self) {
        for project in &mut self.sidebar_model.projects {
            let mut rows: Vec<_> = self
                .worktree_operations
                .iter()
                .filter(|(_, operation)| operation.project == project.id)
                .filter_map(|(&id, operation)| {
                    let (phase, detail) = operation.row(&project.worktrees)?;
                    Some(WorktreeOperationRow {
                        id,
                        label: operation.label.clone(),
                        detail,
                        phase,
                        cancellable: !operation.finished
                            && !operation.termination_requested
                            && operation.token.is_some(),
                        reopenable: operation.reopenable_draft().is_some(),
                    })
                })
                .collect();
            rows.sort_by_key(|row| row.id);
            project.operations = rows;
        }
        self.sidebar_model.apply_worktree_creation_times(
            self.worktree_operations.values().filter_map(|operation| {
                operation
                    .created_worktree
                    .as_ref()
                    .map(|(path, time)| (path, *time))
            }),
        );
        self.sync_worktree_deletion_rows();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retained_cancel_is_once_and_stale_stages_do_not_advance() {
        for stage in [
            None,
            Some(WorktreeProgressStage::RunningGit),
            Some(WorktreeProgressStage::Reconciling),
        ] {
            let mut app = crate::app::tests::context_inspector_test_app(false);
            let id = OperationId::new();
            let token = CancellationToken::new();
            app.worktree_operations.insert(
                id,
                ActiveWorktreeOperation {
                    project: ProjectId::new(),
                    created_worktree: None,
                    generation: 2,
                    request: None,
                    token: Some(token.clone()),
                    receiver: None,
                    stage,
                    started: Instant::now(),
                    termination_requested: false,
                    finished: false,
                    detail: "accepted".into(),
                    completed_elapsed: None,
                    restored: false,
                    label: "review".into(),
                    state: None,
                    draft: None,
                },
            );
            let _ = app.cancel_worktree_operation(id);
            let _ = app.cancel_worktree_operation(id);
            assert!(token.is_cancelled());
            assert!(app.worktree_operations[&id].termination_requested);
            assert!(!app.worktree_operations[&id].finished);
            let _ = app.worktree_progress(WorktreeProgressEvent {
                operation_id: id,
                generation: 1,
                stage: WorktreeProgressStage::Finished,
            });
            assert_eq!(app.worktree_operations[&id].stage, stage);
            let _ = app.finish_worktree_operation(id, 1, Err("stale worker".into()));
            assert!(!app.worktree_operations[&id].finished);
            let _ = app.worktree_progress(WorktreeProgressEvent {
                operation_id: id,
                generation: 2,
                stage: WorktreeProgressStage::Reconciling,
            });
            assert_eq!(
                app.worktree_operations[&id].stage,
                Some(WorktreeProgressStage::Reconciling)
            );
            let _ =
                app.finish_worktree_operation(id, 2, Err("worker lost; reconcile journal".into()));
            assert!(app.worktree_operations[&id].finished);
            assert_eq!(
                app.worktree_operations[&id].state,
                Some(WorktreeOperationState::OutcomeUnknown),
                "a lost worker leaves the outcome unknown until the journal settles it"
            );
            assert!(app.worktree_operations[&id].token.is_none());
        }
    }

    /// A deleted worktree is never listed again, so the creation that made it
    /// must not be left waiting for it (#245).
    #[test]
    fn deleting_a_created_worktree_forgets_its_creation() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let project = ProjectId::new();
        app.sidebar_model
            .projects
            .push(crate::sidebar::SidebarProject::new(
                project,
                RepositoryId::new(),
                "repo".into(),
                "/repo".into(),
                Vec::new(),
                Vec::new(),
            ));
        let created = |path: &str| ActiveWorktreeOperation {
            project,
            created_worktree: Some((PathBuf::from(path), 1)),
            generation: 1,
            request: None,
            token: None,
            receiver: None,
            stage: Some(WorktreeProgressStage::Finished),
            started: Instant::now(),
            termination_requested: false,
            finished: true,
            detail: "created".into(),
            completed_elapsed: None,
            restored: false,
            label: path.into(),
            state: Some(WorktreeOperationState::Created),
            draft: None,
        };
        let deleted = OperationId::new();
        let other = OperationId::new();
        app.worktree_operations
            .insert(deleted, created("/repo/worktrees/deleted"));
        app.worktree_operations
            .insert(other, created("/repo/worktrees/other"));
        app.sync_worktree_operation_rows();
        assert_eq!(app.sidebar_model.projects[0].operations.len(), 2);

        app.forget_worktree_creation(Path::new("/repo/worktrees/deleted"));
        assert!(!app.worktree_operations.contains_key(&deleted));
        assert!(app.worktree_operations.contains_key(&other));
        assert_eq!(
            app.sidebar_model.projects[0]
                .operations
                .iter()
                .map(|row| row.id)
                .collect::<Vec<_>>(),
            vec![other]
        );
    }
}
