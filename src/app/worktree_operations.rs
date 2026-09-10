use super::*;
use crate::{
    model::OperationId,
    repository::{WorktreeOperationReport, WorktreeProgressEvent, WorktreeProgressStage},
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
        WorktreeProgressStage::Validating => "validating",
        WorktreeProgressStage::ResolvingBase => "resolving_base",
        WorktreeProgressStage::RunningGit => "running_git",
        WorktreeProgressStage::Reconciling => "reconciling",
        WorktreeProgressStage::Finished => "finished; awaiting durable outcome",
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

    pub(super) fn start_worktree_input(
        &mut self,
        project_id: ProjectId,
        input: crate::repository::WorktreeInput,
    ) -> Task<Message> {
        let Some(dialog) = &mut self.worktree_dialog else {
            return Task::none();
        };
        if dialog.project_id != project_id || dialog.resolving.is_some() {
            return Task::none();
        }
        let operation_id = input.operation_id;
        dialog.resolving = Some(operation_id);
        dialog.error = None;
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || {
                RepositoryService::system("git")
                    .resolve_worktree_request(&input, &CancellationToken::new())
                    .map_err(|error| error.to_string())
            })
            .await
            .map_err(|error| error.to_string())
            .and_then(|result| result);
            Message::WorktreeInputResolved {
                project_id,
                operation_id,
                result,
            }
        })
    }

    pub(super) fn finish_worktree_input(
        &mut self,
        project_id: ProjectId,
        operation_id: OperationId,
        result: Result<CreateWorktreeRequest, String>,
    ) -> Task<Message> {
        let Some(dialog) = &mut self.worktree_dialog else {
            return Task::none();
        };
        if dialog.project_id != project_id || dialog.resolving != Some(operation_id) {
            return Task::none();
        }
        dialog.resolving = None;
        match result {
            Ok(request) => {
                self.worktree_dialog = None;
                let submit = self.start_worktree_operation(project_id, request);
                Task::batch([submit, self.focus_context_overlay_if_exposed()])
            }
            Err(error) => {
                dialog.error = Some(error.clone());
                self.status = Some(TransientNotice::error(error));
                iced::widget::operation::focus(worktree_dialog_focus_id()).map(cosmic::Action::App)
            }
        }
    }

    pub(super) fn start_worktree_operation(
        &mut self,
        project: ProjectId,
        request: CreateWorktreeRequest,
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
                request: Some(request.clone()),
                token: Some(token.clone()),
                receiver: Some(receiver),
                stage: None,
                started: Instant::now(),
                termination_requested: false,
                finished: false,
                detail: "Accepted; waiting for the durable service".into(),
                completed_elapsed: None,
                restored: false,
                created_worktree: None,
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
                operation.detail = format!("{:?}: {}", report.state, report.detail);
                report.outcome
            }
            Err(error) => {
                operation.detail = format!("OutcomeUnknown: {error}");
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
                            request: report.request,
                            token: None,
                            receiver: None,
                            stage: None,
                            started: Instant::now(),
                            termination_requested: false,
                            finished: true,
                            completed_elapsed: None,
                            restored: true,
                            detail: format!(
                                "Restored {:?}: {} (journaled at {})",
                                report.state, report.detail, report.created_at_unix_ms
                            ),
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
            project.operations = self
                .worktree_operations
                .iter()
                .filter(|(_, operation)| operation.project == project.id)
                .map(|(&id, operation)| crate::sidebar::WorktreeOperationRow {
                    id,
                    label: format!(
                        "{} — {}{}{}",
                        id,
                        operation.detail,
                        if operation.termination_requested {
                            "; termination requested; reconciliation determines the outcome"
                        } else {
                            ""
                        },
                        if operation.restored {
                            String::new()
                        } else {
                            format!(
                                " — elapsed {}s",
                                operation
                                    .completed_elapsed
                                    .unwrap_or_else(|| operation.started.elapsed())
                                    .as_secs()
                            )
                        }
                    ),
                    cancellable: !operation.finished
                        && !operation.termination_requested
                        && operation.token.is_some(),
                })
                .collect();
            project.operations.sort_by_key(|row| row.id);
        }
        self.sidebar_model.apply_worktree_creation_times(
            self.worktree_operations.values().filter_map(|operation| {
                operation
                    .created_worktree
                    .as_ref()
                    .map(|(path, time)| (path, *time))
            }),
        );
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
            assert!(
                app.worktree_operations[&id]
                    .detail
                    .starts_with("OutcomeUnknown")
            );
            assert!(app.worktree_operations[&id].token.is_none());
        }
    }
}
