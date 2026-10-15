//! Root availability is independent of Git. Missing projects use a cheap
//! filesystem probe until restored; no Git/GitHub polling is scheduled for them.
use super::*;
use crate::{
    project_recovery::{self, ProjectInstance, Relocation},
    sidebar::ProjectRecoveryAction,
};

#[derive(Default)]
pub(super) struct State {
    checking: bool,
    operations: HashMap<ProjectId, PendingProjectOperation>,
    locating: HashSet<ProjectId>,
    location_generations: HashMap<ProjectId,uuid::Uuid>,
    // Refusal only: the exact unresolved window capability follows this local
    // project through Locate. It grants no retry, filesystem or start authority.
    relocated_recovery: HashMap<ProjectId, crate::editor::RecoveryAuthority>,
    // The original App publisher may deliver recovery after the logical move.
    // Only its exact captured ticket can attribute that late refusal marker.
    relocation_publications: HashMap<u64, HashSet<ProjectId>>,
    pub(super) closing_panes: bool,
    details: HashSet<CardId>,
}
#[derive(Debug,Clone)]
struct PendingProjectOperation {
    request: crate::project_recovery::ProjectOperationRequest,
    completion: OperationCompletion,
    in_flight: bool,
}
#[derive(Debug,Clone)]
enum OperationCompletion {
    Locate {generation:uuid::Uuid,relocation:Relocation},
    Remove,
}
#[derive(Debug, Clone)]
pub enum Action {
    Tick,
    OperationFinished(ProjectId,uuid::Uuid,Result<crate::project_recovery::ProjectOperationOutcome,String>),
    Checked(Vec<(ProjectId, PathBuf, PathBuf, Option<String>)>),
    AvailabilityChecked(Vec<(ProjectId, PathBuf, PathBuf, Option<String>, bool)>),
    Command(ProjectId, ProjectRecoveryAction),
    VerifyProject(ProjectId),
    SessionDetails(CardId),
    Located(ProjectId, ProjectInstance, PathBuf, uuid::Uuid, Result<PathBuf, String>),
    Prepared(ProjectId, ProjectInstance, uuid::Uuid, Result<Relocation, String>),
    Applied(
        ProjectId,
        ProjectInstance,
        uuid::Uuid,
        Relocation,
        Result<crate::workspace_model::WorkspaceModel, String>,
    ),
}

fn root(project: &SidebarProject) -> PathBuf {
    if project.kind == ProjectKind::Git
        && project
            .repository
            .file_name()
            .is_some_and(|name| name == ".git")
    {
        project
            .repository
            .parent()
            .unwrap_or(&project.repository)
            .to_path_buf()
    } else {
        project.repository.clone()
    }
}

impl Cagents {
    pub(super) fn submit_project_removal_operation(&mut self,id:ProjectId,request:crate::project_recovery::ProjectOperationRequest)->Task<Message> {
        self.install_project_operation(id,request,OperationCompletion::Remove)
    }
    fn install_project_operation(&mut self,id:ProjectId,request:crate::project_recovery::ProjectOperationRequest,completion:OperationCompletion)->Task<Message> {
        if self.project_recovery_ui.operations.contains_key(&id) {
            self.status=Some(TransientNotice::warning("The original project operation is still retained. Use Retry operation; its sources will not be replaced."));
            return Task::none();
        }
        self.project_recovery_ui.operations.insert(id,PendingProjectOperation{request,completion,in_flight:false});
        self.retry_project_operation(id)
    }
    fn retry_project_operation(&mut self,id:ProjectId)->Task<Message> {
        let Some(pending)=self.project_recovery_ui.operations.get_mut(&id) else {return Task::none();};
        if pending.in_flight {return Task::none();}
        if self.workspace_model.identity!=pending.request.project.registry {
            self.status=Some(TransientNotice::warning("This retained operation belongs to its original registry. Reconnect that registry to retry; no sources were recaptured."));
            return Task::none();
        }
        pending.in_flight=true;
        let request=pending.request.clone();
        let operation=request.operation;
        if let Some(row)=self.sidebar_model.projects.iter_mut().find(|row|row.id==id) {
            row.pending_operation=Some(crate::sidebar::ProjectOperationNotice{operation,running:true,reason:"Finishing the original project operation…".into()});
        }
        cosmic::task::future(async move {
            let result=tokio::task::spawn_blocking(move || {
                let socket=crate::terminal::ensure_daemon_ready().map_err(|error|error.to_string())?;
                crate::control::ControlClient::connect(&socket).map_err(|error|error.to_string())?
                    .project_operation(request).map_err(|error|error.to_string())
            }).await.map_err(|error|error.to_string()).and_then(|result|result);
            Message::ProjectRecovery(Action::OperationFinished(id,operation,result))
        })
    }
    pub(super) fn remember_project_binding(
        &mut self,
        id: ProjectId,
        shared: &crate::workspace_model::ModelProject,
    ) {
        if shared.ordinal == crate::workspace_model::PENDING_ORDINAL {
            return;
        }
        let root = shared
            .primary_checkout
            .clone()
            .or_else(|| shared.checkout.clone())
            .or_else(|| (shared.kind == ProjectKind::Folder).then(|| shared.repository.clone()));
        let binding = self.durable.shared_project_bindings.get(&id);
        let next = match binding {
            None => Some(crate::model::SharedProjectBinding {
                ordinal: shared.ordinal,
                root,
            }),
            Some(previous)
                if previous.ordinal == shared.ordinal
                    && previous.root.is_none()
                    && root.is_some() =>
            {
                Some(crate::model::SharedProjectBinding {
                    ordinal: shared.ordinal,
                    root,
                })
            }
            _ => None,
        };
        if let Some(next) = next {
            Arc::make_mut(&mut self.durable)
                .shared_project_bindings
                .insert(id, next);
            self.persist();
        }
    }

    pub(super) fn project_root(&self, project: &SidebarProject) -> PathBuf {
        self.durable
            .shared_project_bindings
            .get(&project.id)
            .and_then(|binding| binding.root.clone())
            .or_else(|| {
                self.project_ui
                    .primary_checkout_roots
                    .get(&project.repository)
                    .cloned()
            })
            .or_else(|| {
                self.project_ui
                    .checkout_roots
                    .get(&project.repository)
                    .cloned()
            })
            .unwrap_or_else(|| root(project))
    }
    pub(super) fn project_missing(&self, project: ProjectId) -> bool {
        self.sidebar_model
            .project(project)
            .is_some_and(|project| project.missing)
    }
    pub(super) fn project_cards(&self, id: ProjectId) -> HashSet<CardId> {
        let Some(project) = self.sidebar_model.project(id) else {
            return HashSet::new();
        };
        let owned_roots = std::iter::once(self.project_root(project))
            .chain(std::iter::once(project.repository.clone()))
            .chain(
                project
                    .worktrees
                    .iter()
                    .map(|worktree| worktree.path.clone()),
            )
            .collect::<Vec<_>>();
        let protected = self
            .sidebar_model
            .projects
            .iter()
            .filter(|project| project.id != id)
            .flat_map(|project| {
                std::iter::once(self.project_root(project))
                    .chain(std::iter::once(project.repository.clone()))
                    .chain(
                        project
                            .worktrees
                            .iter()
                            .map(|worktree| worktree.path.clone()),
                    )
            })
            .collect::<Vec<_>>();
        let owns_path = |path: &Path| {
            let depth = |roots: &[PathBuf]| {
                roots
                    .iter()
                    .filter(|root| path.starts_with(root))
                    .map(|root| root.components().count())
                    .max()
            };
            match (depth(&owned_roots), depth(&protected)) {
                (Some(owned), Some(other)) => owned > other,
                (Some(_), None) => true,
                _ => false,
            }
        };
        let channel_owners = self
            .durable
            .channels
            .iter()
            .flat_map(|channel| {
                channel
                    .cards
                    .iter()
                    .map(move |card| (*card, channel.project_id))
            })
            .collect::<HashMap<_, _>>();
        self.durable
            .cards
            .iter()
            .filter(|card| {
                if let Some(project) = channel_owners.get(&card.id) {
                    return *project == id;
                }
                match &card.kind {
                    CardKind::Repository(view) => view.project == id,
                    CardKind::PullRequests(view) => view.repository == project.repository,
                    CardKind::Terminal(session) => {
                        self.durable.card_scopes.get(&card.id).map_or_else(
                            || owns_path(&session.launch.working_directory),
                            |path| owns_path(path),
                        )
                    }
                    CardKind::Chat(session) => self.durable.card_scopes.get(&card.id).map_or_else(
                        || owns_path(&session.launch.recipe.working_directory),
                        |path| owns_path(path),
                    ),
                    _ => self
                        .durable
                        .card_scopes
                        .get(&card.id)
                        .is_some_and(|path| owns_path(path)),
                }
            })
            .map(|card| card.id)
            .collect()
    }
    fn project_publication_recovery_roots(&self, project: ProjectId) -> HashSet<PathBuf> {
        let Some(row)=self.sidebar_model.project(project) else {return HashSet::new();};
        std::iter::once(self.project_root(row))
            .chain(std::iter::once(row.repository.clone()))
            .chain(row.worktrees.iter().map(|worktree|worktree.path.clone()))
            .chain(self.project_ui.checkout_roots.get(&row.repository).cloned())
            .chain(self.project_ui.primary_checkout_roots.get(&row.repository).cloned())
            .collect()
    }
    fn capture_project_publication_recovery(&mut self, project: ProjectId) {
        let current=self.sidebar_model.projects.iter().map(|row|row.id).collect::<HashSet<_>>();
        self.project_recovery_ui.relocated_recovery.retain(|id,authority|current.contains(id) && !authority.is_resolved());
        let cards=self.project_cards(project);
        if let Some(ticket)=self.pending_publication_ticket_for_project(project,&cards) {
            self.project_recovery_ui.relocation_publications.entry(ticket).or_default().insert(project);
        }
        for root in self.project_publication_recovery_roots(project) {
            let Some(authority)=self.publication_recovery_authority(&root) else {continue;};
            let authority=self.project_recovery_ui.relocated_recovery.remove(&project)
                .map_or_else(||authority.clone(),|previous|previous.combine(authority.clone()));
            self.project_recovery_ui.relocated_recovery.insert(project,authority);
        }
    }
    /// Called for every actual App publisher result before its stale-owner
    /// return. No source path is inspected or re-attributed at completion.
    pub(super) fn retain_relocated_publication_recovery(&mut self, ticket: u64, authority: Option<&crate::editor::RecoveryAuthority>) {
        let Some(projects)=self.project_recovery_ui.relocation_publications.remove(&ticket) else {return;};
        let Some(authority)=authority.filter(|authority|!authority.is_resolved()) else {return;};
        for project in projects {
            if self.sidebar_model.project(project).is_none() {continue;}
            let authority=self.project_recovery_ui.relocated_recovery.remove(&project)
                .map_or_else(||authority.clone(),|previous|previous.combine(authority.clone()));
            self.project_recovery_ui.relocated_recovery.insert(project,authority);
        }
    }
    /// Publisher intake supplies its actual origin and participant cards before
    /// lease/freeze/I/O. A new operation cannot slip behind a chooser's capture.
    pub(super) fn project_relocation_blocks_publication(&self, cards: &HashSet<CardId>) -> bool {
        self.project_recovery_ui.location_generations.keys().any(|project| {
            self.project_cards(*project).iter().any(|card|cards.contains(card))
        })
    }
    pub(super) fn project_retains_relocated_recovery(&self, project: ProjectId) -> bool {
        self.sidebar_model.project(project).is_some()
            && self.project_recovery_ui.relocated_recovery.get(&project).is_some_and(|authority|!authority.is_resolved())
    }
    pub(super) fn project_worktree_publication_recovery(&self, project: ProjectId, path: &Path) -> bool {
        self.project_retains_relocated_recovery(project) || self.publication_recovery_blocks(path)
    }
    pub(super) fn project_has_publication_recovery(&self, project: ProjectId) -> bool {
        self.project_retains_relocated_recovery(project)
            || self.project_publication_recovery_roots(project).iter().any(|root|self.publication_recovery_blocks(root))
    }
    pub(super) fn project_has_editor_recovery(&self, project: ProjectId) -> bool {
        self.project_has_publication_recovery(project)
            || self.project_cards(project).into_iter().any(|card| self.card_has_editor_recovery(card))
    }
    pub(super) fn missing_project_cards(&self) -> HashSet<CardId> {
        self.sidebar_model
            .projects
            .iter()
            .filter(|project| project.missing)
            .flat_map(|project| self.project_cards(project.id))
            .collect()
    }
    pub(super) fn recovery_subscription(&self) -> Subscription<Message> {
        if crate::device::is_remote() || self.sidebar_model.projects.is_empty() {
            return Subscription::none();
        }
        cosmic::iced::time::every(Duration::from_secs(2))
            .map(|_| Message::ProjectRecovery(Action::Tick))
    }
    pub(super) fn update_project_recovery(&mut self, action: Action) -> Task<Message> {
        // Preserve the direct probe fixture/API while the live worker also
        // verifies alternative checkouts before declaring a whole Git project missing.
        let action=match action {
            Action::Checked(projects)=>Action::AvailabilityChecked(projects.into_iter().map(|(id,path,root,reason)| {
                let available=reason.is_none(); (id,path,root,reason,available)
            }).collect()),
            action=>action,
        };
        match action {
            Action::Command(id,ProjectRecoveryAction::RetryOperation(operation)) => {
                if self.modal_is_open() || self.project_recovery_ui.operations.get(&id).is_none_or(|pending|pending.request.operation!=operation) {return Task::none();}
                return self.retry_project_operation(id);
            }
            Action::OperationFinished(id,operation,result) => {
                let Some(pending)=self.project_recovery_ui.operations.get_mut(&id).filter(|pending|pending.request.operation==operation && pending.in_flight) else {return Task::none();};
                pending.in_flight=false;
                let model=match result {
                    Ok(crate::project_recovery::ProjectOperationOutcome::Committed{operation:received,model}) if received==operation=>Some(model),
                    result=> {
                        let reason=match result {
                            Ok(crate::project_recovery::ProjectOperationOutcome::Pending{operation:received,reason,..}) if received==operation=>reason,
                            Ok(_)=>"The daemon returned a different operation identity. The original request remains retained.".into(),
                            Err(error)=>format!("The outcome is unavailable: {error}. The original operation remains retained; Retry operation sends exactly the same request."),
                        };
                        if let Some(row)=self.sidebar_model.projects.iter_mut().find(|row|row.id==id) {
                            row.pending_operation=Some(crate::sidebar::ProjectOperationNotice{operation,running:false,reason:reason.clone()});
                        }
                        self.status=Some(TransientNotice::warning(reason));None
                    }
                };
                let Some(model)=model else {return Task::none();};
                let pending=self.project_recovery_ui.operations.remove(&id).unwrap();
                if let Some(row)=self.sidebar_model.projects.iter_mut().find(|row|row.id==id) {row.pending_operation=None;}
                match pending.completion {
                    OperationCompletion::Locate{generation,relocation}=>return self.update_project_recovery(Action::Applied(id,pending.request.project,generation,relocation,Ok(model))),
                    OperationCompletion::Remove=>return self.handle_project_action(project_ui::Action::RemovedProject{repository:pending.request.project.repository.clone(),expected:pending.request.project,result:Ok(model)}),
                }
            }
            Action::Tick => {
                if self.project_recovery_ui.checking {
                    return Task::none();
                }
                let projects = self
                    .sidebar_model
                    .projects
                    .iter()
                    .map(|project| {
                        (
                            project.id,
                            project.repository.clone(),
                            self.project_root(project),
                            project.kind,
                            self.project_ui.checkout_roots.get(&project.repository).cloned(),
                            project.worktrees.iter().map(|worktree|worktree.path.clone()).collect::<Vec<_>>(),
                        )
                    })
                    .collect::<Vec<_>>();
                self.project_recovery_ui.checking = true;
                cosmic::task::future(async move {
                    let checked = tokio::task::spawn_blocking(move || {
                        projects
                            .into_iter()
                            .map(|(id, path, root, kind, checkout, worktrees)| {
                                let reason=project_recovery::missing_reason(&root);
                                let available=reason.is_none() || kind==ProjectKind::Git && (
                                    crate::project_location::read_checkout(&path,checkout.as_deref(),Some(&root)).is_some()
                                    || worktrees.iter().any(|worktree|crate::project_location::read_checkout(&path,Some(worktree),None).is_some())
                                );
                                (id,path,root,reason,available)
                            })
                            .collect()
                    })
                    .await
                    .unwrap_or_default();
                    Message::ProjectRecovery(Action::AvailabilityChecked(checked))
                })
            }
            Action::Checked(_) => unreachable!("direct probes normalize to availability results"),
            Action::AvailabilityChecked(projects) => {
                self.project_recovery_ui.checking = false;
                let mut tasks = Vec::new();
                for (id, path, probed_root, reason, available) in projects {
                    let missing = reason.is_some() && !available;
                    let known_root = self
                        .sidebar_model
                        .project(id)
                        .map(|project| self.project_root(project));
                    if known_root.as_ref()!=Some(&probed_root) {
                        // Same-key external-gitdir Locate can keep id/repository
                        // while changing the root. Old probes cannot cancel its
                        // replacement reads or apply a stale repair warning.
                        continue;
                    }
                    let Some(project) = self
                        .sidebar_model
                        .projects
                        .iter_mut()
                        .find(|project| project.id == id && project.repository == path)
                    else {
                        continue;
                    };
                    let detail = reason.map(|reason| {
                        format!(
                            "{}: {reason}",
                            known_root.as_deref().unwrap_or(&path).display()
                        )
                    });
                    let repair=(!missing).then(||detail.clone()).flatten();
                    if repair.is_some() && project.location_repair!=repair {
                        // A read against the vanished primary may have failed
                        // before this probe found its usable linked checkout.
                        // Let normal refresh retry via the verified fallback;
                        // do not repeatedly erase a later fallback read failure.
                        project.unreachable=None;
                    }
                    project.location_repair=repair;
                    if project.missing == missing {
                        if missing {
                            project.unreachable = detail;
                        }
                        continue;
                    }
                    project.missing = missing;
                    if missing {
                        project.unreachable = detail;
                        project.reloading = false;
                        self.refresh_ui.git.forget(id);
                        self.repository_generations.remove(&id);
                        if let Some(cancel) = self.refresh_ui.projects.remove(&id) {
                            cancel.cancel();
                        }
                        if let Some(cancel) = self.repository_pr_cancellations.remove(&id) {
                            cancel.cancel();
                        }
                        if let Some(key) = self
                            .sidebar_state
                            .active_worktree()
                            .cloned()
                            .filter(|key| key.project_id == id)
                        {
                            self.cancel_source_read();
                            self.sidebar_model.source_control = Some(SourceControlModel::failed(
                                key,
                                "This project folder was moved or deleted. Locate it from Projects.",
                            ));
                        }
                    } else {
                        project.unreachable = None;
                        tasks.push(self.project_load_task(path, Some(id), false, None));
                    }
                    tasks.push(self.project_search_availability_changed(id));
                }
                Task::batch(tasks)
            }
            Action::VerifyProject(id) => {
                if self.modal_is_open() || !self.project_recovery_ui.locating.insert(id) {
                    return Task::none();
                }
                let Some(project) = self.sidebar_model.project(id) else {
                    self.finish_project_location(id);
                    return Task::none();
                };
                let Some(expected)=self.project_instance(id) else { self.finish_project_location(id); self.status=Some(TransientNotice::warning("Refresh Projects before verifying its location.")); return Task::none(); };
                let root = self.project_root(project);
                let repository=project.repository.clone();
                self.capture_project_publication_recovery(id);
                let generation=uuid::Uuid::new_v4();
                self.project_recovery_ui.location_generations.insert(id,generation);
                let relocation = Relocation {
                    repository: repository.clone(),
                    destination: repository,
                    old_root: root.clone(),
                    new_root: root,
                };
                self.update_project_recovery(Action::Prepared(id, expected, generation, Ok(relocation)))
            }
            Action::SessionDetails(card) => {
                if !self.project_recovery_ui.details.insert(card) {
                    self.project_recovery_ui.details.remove(&card);
                }
                Task::none()
            }
            Action::Command(id, ProjectRecoveryAction::Details) => {
                if let Some(project) = self
                    .sidebar_model
                    .projects
                    .iter_mut()
                    .find(|project| project.id == id)
                {
                    project.missing_details = !project.missing_details;
                }
                Task::none()
            }
            Action::Command(id, ProjectRecoveryAction::Remove) => {
                self.handle_project_action(project_ui::Action::Command {
                    project_id: id,
                    worktree_id: None,
                    command: sidebar::ProjectCommand::Remove,
                })
            }
            Action::Command(id, ProjectRecoveryAction::ClosePanes) => {
                let owned = self.project_cards(id);
                let cards = self
                    .durable
                    .cards
                    .iter()
                    .filter(|card| card.existence.is_open() && owned.contains(&card.id))
                    .map(|card| card.id)
                    .collect();
                self.close_cards_in_order(cards)
            }
            Action::Command(id, ProjectRecoveryAction::Locate) => {
                if !self.project_recovery_ui.locating.insert(id) {
                    return Task::none();
                }
                let Some(expected)=self.project_instance(id) else { self.finish_project_location(id); self.status=Some(TransientNotice::warning("Refresh Projects before locating its folder.")); return Task::none(); };
                let original_root=self.project_root(self.sidebar_model.project(id).unwrap());
                self.capture_project_publication_recovery(id);
                let generation=uuid::Uuid::new_v4();
                self.project_recovery_ui.location_generations.insert(id,generation);
                if let Some(project)=self.sidebar_model.projects.iter_mut().find(|project|project.id==id) { project.reloading=true; }
                cosmic::task::future(async move {
                    let result = file_chooser::open::Dialog::new()
                        .title("Locate project folder")
                        .open_folder()
                        .await
                        .map_err(|error| error.to_string())
                        .and_then(|response| {
                            response
                                .url()
                                .to_file_path()
                                .map_err(|()| "Select a local folder".into())
                        });
                    Message::ProjectRecovery(Action::Located(id, expected, original_root, generation, result))
                })
            }
            Action::Located(id, expected, original_root, generation, result) => {
                if self.project_recovery_ui.location_generations.get(&id)!=Some(&generation) { return Task::none(); }
                if self.project_instance(id).as_ref()!=Some(&expected) || self.sidebar_model.project(id).is_none_or(|project|self.project_root(project)!=original_root) {
                    self.finish_project_location(id);
                    self.status=Some(TransientNotice::warning("The original project changed while its folder chooser was open. Refresh Projects and locate it again."));
                    return Task::none();
                }
                let path = match result {
                    Ok(path) => path,
                    Err(error) => {
                        self.finish_project_location(id);
                        if !error.to_lowercase().contains("cancel") {
                            self.status = Some(TransientNotice::error(error));
                        }
                        return Task::none();
                    }
                };
                let Some(project) = self.sidebar_model.project(id) else {
                    self.finish_project_location(id);
                    return Task::none();
                };
                let old_root = self.project_root(project);
                let repository = project.repository.clone();
                let kind = project.kind;
                let open_repositories = self
                    .sidebar_model
                    .projects
                    .iter()
                    .filter(|project| project.id != id)
                    .map(|project| project.repository.clone())
                    .collect::<Vec<_>>();
                cosmic::task::future(async move {
                    let result = tokio::task::spawn_blocking(move || {
                        let new_root = path.canonicalize().map_err(|error| error.to_string())?;
                        let destination = if kind == ProjectKind::Folder { new_root.clone() } else {
                            crate::project_location::checkout_identity(&new_root).map(|(_, common)| common).ok_or("Choose the root folder of a Git checkout for this project")?
                        };
                        if open_repositories.contains(&destination) { return Err("That folder is already open as another project".into()); }
                        let relocation = Relocation { repository, destination, old_root, new_root };
                        relocation.validate()?;
                        Ok(relocation)
                    }).await.map_err(|error| error.to_string()).and_then(|result| result);
                    Message::ProjectRecovery(Action::Prepared(id, expected, generation, result))
                })
            }
            Action::Prepared(id, expected, generation, result) => {
                if self.project_recovery_ui.location_generations.get(&id)!=Some(&generation) { return Task::none(); }
                if self.project_instance(id).as_ref()!=Some(&expected) {
                    self.finish_project_location(id);
                    self.status=Some(TransientNotice::warning("The original project was replaced while locating it. Refresh Projects and try again."));
                    return Task::none();
                }
                let relocation = match result {
                    Ok(relocation) => relocation,
                    Err(error) => {
                        self.finish_project_location(id);
                        self.status = Some(TransientNotice::error(error));
                        return Task::none();
                    }
                };
                if !self
                    .sidebar_model
                    .projects
                    .iter()
                    .any(|project| project.id == id && project.repository == relocation.repository && self.project_root(project)==relocation.old_root)
                {
                    self.finish_project_location(id);
                    return Task::none();
                }
                if self
                    .sidebar_model
                    .projects
                    .iter()
                    .any(|project| project.id != id && project.repository == relocation.destination)
                {
                    self.finish_project_location(id);
                    self.status = Some(TransientNotice::warning(
                        "That folder is already open as another project",
                    ));
                    return Task::none();
                }
                if self.cards.iter().any(|(card, runtime)| {
                    matches!(runtime, CardRuntime::Repository(runtime) if runtime.is_saving())
                        && self
                            .durable
                            .card_scopes
                            .get(card)
                            .is_some_and(|path| path.starts_with(&relocation.old_root))
                }) {
                    self.finish_project_location(id);
                    self.status = Some(TransientNotice::warning(
                        "Wait for the current save to finish, then locate the project again.",
                    ));
                    return Task::none();
                }
                let request=crate::project_recovery::ProjectOperationRequest {
                    operation:generation,project:expected,
                    action:crate::project_recovery::ProjectOperationAction::Locate{relocation:relocation.clone()},
                };
                self.install_project_operation(id,request,OperationCompletion::Locate{generation,relocation})
            }
            Action::Applied(id, expected, generation, relocation, result) => {
                if self.project_recovery_ui.location_generations.get(&id)!=Some(&generation) { return Task::none(); }
                self.finish_project_location(id);
                if self.project_instance(id).as_ref()!=Some(&expected) { return Task::none(); }
                match result {
                    Ok(model) => {
                        if model.project(&relocation.destination).is_none() {
                            self.status = Some(TransientNotice::warning(
                                "The project location changed while locating it. Refresh and try again.",
                            ));
                            return Task::none();
                        }
                        if model.identity == self.workspace_model.identity
                            && model.sequence < self.workspace_model.sequence
                        {
                            return self.apply_project_relocation_retained(&relocation);
                        }
                        self.workspace_model = model;
                        let relocated = if relocation.verification_only() {
                            Task::none()
                        } else {
                            self.apply_project_relocation_retained(&relocation)
                        };
                        let refused = self
                            .project_cards(id)
                            .into_iter()
                            .filter(|card| self.card_location_refusal(*card).is_some())
                            .count();
                        self.status = Some(if refused > 0 {
                            TransientNotice::warning(format!(
                                "Project located. {refused} panes still need directory verification; use Verify again or Close pane in each warning."
                            ))
                        } else if relocation.verification_only() {
                            TransientNotice::success("Session locations verified.")
                        } else {
                            TransientNotice::success(
                                "Project located. Existing schedules are paused until their target is updated.",
                            )
                        });
                        relocated
                    }
                    Err(error) => {
                        self.status = Some(TransientNotice::error(format!(
                            "Could not locate project: {error}"
                        )));
                        Task::none()
                    }
                }
            }
        }
    }

    fn finish_project_location(&mut self, id: ProjectId) {
        self.project_recovery_ui.locating.remove(&id);
        self.project_recovery_ui.location_generations.remove(&id);
        if let Some(project) = self
            .sidebar_model
            .projects
            .iter_mut()
            .find(|project| project.id == id)
        {
            project.reloading = false;
        }
    }

    pub(super) fn card_location_refusal(
        &self,
        card: CardId,
    ) -> Option<&crate::workspace_model::SessionLocationRefusal> {
        let state = self.card_state(card)?;
        if !state.existence.is_open() {
            return None;
        }
        let session = match &state.kind {
            CardKind::Chat(session) => session.id,
            CardKind::Terminal(session) => session.id,
            _ => return None,
        };
        self.workspace_model
            .session_location_refusals
            .iter()
            .find(|refusal| refusal.session == session)
    }

    pub(super) fn project_location_notice(&self, card: CardId) -> Option<Element<'_, Message>> {
        let refusal = self.card_location_refusal(card)?;
        let expanded = self.project_recovery_ui.details.contains(&card);
        let project = self
            .durable
            .channels
            .iter()
            .find(|channel| channel.cards.contains(&card))
            .map(|channel| channel.project_id)
            .or_else(|| {
                self.durable
                    .card_scopes
                    .get(&card)
                    .and_then(|path| self.sidebar_model.worktree_at(path))
                    .map(|key| key.project_id)
            });
        let enabled = !self.modal_is_open();
        let mut actions = Vec::new();
        if let Some(project) = project {
            actions.push(
                crate::ui_controls::button::quiet("Verify again")
                    .id(card_control_id(card, "project.location.retry"))
                    .on_press_maybe(
                        (enabled && !self.project_recovery_ui.locating.contains(&project))
                            .then_some(Message::ProjectRecovery(Action::VerifyProject(project))),
                    )
                    .into(),
            );
        }
        actions.push(
            crate::ui_controls::button::quiet("Close pane")
                .on_press_maybe(enabled.then_some(Message::CloseCard(card)))
                .into(),
        );
        actions.push(
            crate::ui_controls::button::quiet(if expanded { "Hide details" } else { "Details" })
                .on_press_maybe(
                    enabled.then_some(Message::ProjectRecovery(Action::SessionDetails(card))),
                )
                .into(),
        );
        let mut body = widget::column::with_children(vec![
            notice_body("This session still needs a verified directory."),
            notice_surface_detail(
                NoticeSeverity::Warning,
                if expanded {
                    format!("{}\n{}", refusal.directory.display(), refusal.reason)
                } else {
                    let mut reason = refusal.reason.chars().take(160).collect::<String>();
                    if refusal.reason.chars().count() > 160 {
                        reason.push('…');
                    }
                    reason
                },
            ),
        ])
        .spacing(space::XS)
        .width(Length::Fill);
        body = body.push(widget::row::with_children(actions).spacing(space::XS));
        Some(notice_surface(
            NoticeSeverity::Warning,
            body.into(),
            None,
            false,
        ))
    }

    /// A reconnect can miss Locate's watch edge. Recover logical ownership
    /// from the non-reused registry project rank or exact channel/agent IDs before
    /// honoring the old project's tombstone. Paths alone prove no association.
    pub(super) fn recover_shared_project_relocations(&mut self) -> Task<Message> {
        if self.workspace_model.identity == crate::workspace_model::NO_REGISTRY
            || self.durable.workspace_model_identity != Some(self.workspace_model.identity)
        {
            return Task::none();
        }
        let mut relocations = Vec::new();
        for project in &self.sidebar_model.projects {
            let mut repositories = HashSet::new();
            if let Some(binding) = self.durable.shared_project_bindings.get(&project.id) {
                repositories.extend(
                    self.workspace_model
                        .projects
                        .iter()
                        .filter(|shared| shared.ordinal == binding.ordinal)
                        .map(|shared| shared.repository.clone()),
                );
            } else {
                // Channels cannot change project association except in an
                // explicit project relocation. A pane ID alone is insufficient:
                // panes can legitimately be transferred between projects.
                repositories.extend(
                    self.durable
                        .channels
                        .iter()
                        .filter(|channel| channel.project_id == project.id && channel.shared)
                        .filter_map(|channel| self.workspace_model.channel(channel.id))
                        .map(|channel| channel.repository.clone()),
                );
                let owned = self.project_cards(project.id);
                for location in &self.workspace_model.agent_locations {
                    if owned.contains(&CardId::from_uuid(location.agent.as_uuid())) {
                        repositories.insert(location.repository.clone());
                    }
                }
            }
            if repositories.len() != 1 {
                continue;
            }
            let destination = repositories.into_iter().next().unwrap();
            let Some(located) = self.workspace_model.project(&destination) else {
                continue;
            };
            let old_root = self.project_root(project);
            // An external metadata key can stay unchanged when its primary
            // moves. Ordinary opens of a linked checkout must not move it.
            if destination == project.repository
                && located
                    .primary_checkout
                    .as_ref()
                    .is_none_or(|root| root == &old_root)
            {
                continue;
            }
            let new_root = located
                .primary_checkout
                .clone()
                .or_else(|| located.checkout.clone())
                .unwrap_or_else(|| {
                    if located.kind == ProjectKind::Git
                        && destination.file_name().is_some_and(|name| name == ".git")
                    {
                        destination.parent().unwrap_or(&destination).to_path_buf()
                    } else {
                        destination.clone()
                    }
                });
            let relocation = Relocation {
                repository: project.repository.clone(),
                destination,
                old_root,
                new_root,
            };
            if relocation.validate().is_ok() {
                relocations.push(relocation);
            }
        }
        Task::batch(
            relocations
                .into_iter()
                .map(|relocation| self.apply_project_relocation(&relocation)),
        )
    }

    pub(super) fn apply_project_relocation(&mut self, relocation: &Relocation) -> Task<Message> {
        // A peer/watch transition has no local chooser intake. Capture its
        // currently attributed capability before changing any logical roots.
        if let Some(id)=self.sidebar_model.projects.iter().find(|row|row.repository==relocation.repository).map(|row|row.id) {
            self.capture_project_publication_recovery(id);
        }
        self.apply_project_relocation_retained(relocation)
    }

    fn apply_project_relocation_retained(&mut self, relocation: &Relocation) -> Task<Message> {
        if relocation.verification_only() {
            return Task::none();
        }
        let Some(id) = self
            .sidebar_model
            .projects
            .iter()
            .find(|project| project.repository == relocation.repository)
            .map(|project| project.id)
        else {
            return Task::none();
        };
        let owned = self.project_cards(id);
        self.invalidate_project_search(id);
        for roots in [
            &mut self.project_ui.checkout_roots,
            &mut self.project_ui.primary_checkout_roots,
        ] {
            if let Some(previous) = roots.remove(&relocation.repository) {
                roots.insert(relocation.destination.clone(), relocation.mapped(&previous));
            }
        }
        if let Some(binding) = Arc::make_mut(&mut self.durable)
            .shared_project_bindings
            .get_mut(&id)
        {
            binding.root = Some(binding.root.as_deref().map_or_else(
                || relocation.new_root.clone(),
                |root| relocation.mapped(root),
            ));
        }
        let project = self
            .sidebar_model
            .projects
            .iter_mut()
            .find(|project| project.id == id)
            .unwrap();
        project.relocate(relocation);
        if let Some(session) = &self.session {
            session.save_project(ProjectState {
                id,
                generation: 0,
                name: project.name.clone(),
                repository: project.repository.clone(),
                kind: project.kind,
                existence: Existence::Open,
            });
            session.save_worktrees(project.worktrees.iter().map(|worktree| WorktreeState {
                id: worktree.id,
                project_id: id,
                generation: 0,
                path: worktree.path.clone(),
                branch: worktree.branch.clone(),
                existence: Existence::Open,
            }));
        }
        self.refresh_ui.git.forget(id);
        let durable = Arc::make_mut(&mut self.durable);
        for card in &mut durable.cards {
            if owned.contains(&card.id) {
                relocation.card(&mut card.kind);
            }
        }
        for (card, path) in &mut durable.card_scopes {
            if owned.contains(card) {
                relocation.path(path);
            }
        }
        for (card, navigation) in &mut durable.repository_navigation {
            if owned.contains(card) {
                relocation.path(&mut navigation.worktree);
            }
        }
        if let Some(path) = &mut durable.active_worktree {
            if self
                .sidebar_state
                .active_worktree()
                .is_some_and(|key| key.project_id == id)
            {
                relocation.path(path);
            }
        }
        durable.worktree_focus = std::mem::take(&mut durable.worktree_focus)
            .into_iter()
            .map(|(path, card)| {
                (
                    if owned.contains(&card) {
                        relocation.mapped(&path)
                    } else {
                        path
                    },
                    card,
                )
            })
            .collect();
        if let Some(path) = &mut self.active_workspace {
            if self
                .sidebar_state
                .active_worktree()
                .is_some_and(|key| key.project_id == id)
            {
                relocation.path(path);
            }
        }
        self.worktree_focus = std::mem::take(&mut self.worktree_focus)
            .into_iter()
            .map(|(path, card)| {
                (
                    if owned.contains(&card) {
                        relocation.mapped(&path)
                    } else {
                        path
                    },
                    card,
                )
            })
            .collect();
        self.known_worktree_roots = std::mem::take(&mut self.known_worktree_roots)
            .into_iter()
            .map(|path| {
                if relocation.old_root == path
                    || self.sidebar_model.project(id).is_some_and(|project| {
                        project
                            .worktrees
                            .iter()
                            .any(|worktree| relocation.mapped(&path) == worktree.path)
                    })
                {
                    relocation.mapped(&path)
                } else {
                    path
                }
            })
            .collect();
        for (card, path) in &mut self.worktree_owners {
            if owned.contains(card) {
                relocation.path(path);
            }
        }
        let mut tasks = Vec::new();
        for (card, runtime) in &mut self.cards {
            if !owned.contains(card) {
                continue;
            }
            match runtime {
                CardRuntime::Repository(runtime) => {
                    tasks.push(repository_task(runtime.relocate_project(*card, relocation)));
                }
                _ => {}
            }
        }
        self.shared_views.relocate(relocation, &owned);
        self.cancel_source_read();
        self.sidebar_model.source_control = None;
        self.persist();
        let editors = self
            .cards
            .iter()
            .filter_map(|(card, runtime)| {
                matches!(runtime, CardRuntime::Repository(_)).then_some(*card)
            })
            .collect::<Vec<_>>();
        tasks.extend(editors.into_iter().map(|card| self.drain_lsp_intents(card)));
        tasks.push(self.project_load_task(relocation.destination.clone(), Some(id), false, None));
        Task::batch(tasks)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn app(root: &Path) -> (Cagents, ProjectId) {
        let mut app = super::super::tests::context_inspector_test_app(false);
        let id = ProjectId::new();
        app.sidebar_model.projects.push(
            SidebarProject::new(
                id,
                RepositoryId::new(),
                "Fixture".into(),
                root.to_path_buf(),
                Vec::new(),
                Vec::new(),
            )
            .with_kind(ProjectKind::Folder),
        );
        let identity=uuid::Uuid::new_v4();
        let durable=Arc::make_mut(&mut app.durable);
        durable.workspace_model_identity=Some(identity);
        durable.shared_project_bindings.insert(id,crate::model::SharedProjectBinding{ordinal:1,root:Some(root.to_owned())});
        app.workspace_model.identity=identity;
        app.workspace_model.projects.push(crate::workspace_model::ModelProject{repository:root.to_owned(),checkout:None,primary_checkout:None,name:"Fixture".into(),kind:ProjectKind::Folder,ordinal:1});
        (app, id)
    }
    #[test]
    fn pending_project_operation_keeps_original_request_across_explicit_retry_and_stale_reply() {
        let fixture=tempfile::tempdir().unwrap();let (mut app,id)=app(fixture.path());
        let expected=app.project_instance(id).unwrap();
        let request=crate::project_recovery::ProjectOperationRequest{operation:uuid::Uuid::new_v4(),project:expected.clone(),action:crate::project_recovery::ProjectOperationAction::Remove{sessions:Vec::new()}};
        // Dropping these actual Tasks avoids daemon startup; this is a reducer
        // ownership/notice test, with framed request replay covered in control.
        drop(app.submit_project_removal_operation(id,request.clone()));
        assert!(app.project_recovery_ui.operations[&id].in_flight);
        drop(app.update_project_recovery(Action::OperationFinished(id,request.operation,Ok(crate::project_recovery::ProjectOperationOutcome::Pending{operation:request.operation,phase:crate::project_recovery::ProjectOperationPhase::PendingCommit,reason:"Original owners closed; final commit is busy".into()}))));
        assert!(!app.project_recovery_ui.operations[&id].in_flight);
        let notice=app.sidebar_model.project(id).unwrap().pending_operation.as_ref().unwrap();
        assert_eq!(notice.operation,request.operation);assert!(!notice.running);assert!(notice.reason.contains("final commit"));
        let expected_notice=notice.clone();
        // Deliver an actual folder worker result through the real App reducer.
        // Ordinary project refresh must not make the sole retry control vanish.
        let loaded=super::super::load_project(fixture.path(),Some(id),CancellationToken::default(),Some(ProjectKind::Folder)).unwrap();
        app.repository_generations.insert(id,1);
        drop(app.update(Message::RepositoryLoaded{open_cards:false,project_id:id,generation:1,result:Ok(Box::new(loaded))}));
        assert_eq!(app.sidebar_model.project(id).unwrap().pending_operation.as_ref(),Some(&expected_notice));
        assert_eq!(app.project_recovery_ui.operations[&id].request,request);
        // Even a newly observed ordinal must not become retry consent.
        app.workspace_model.projects[0].ordinal=99;
        drop(app.update_project_recovery(Action::Command(id,ProjectRecoveryAction::RetryOperation(request.operation))));
        assert_eq!(app.project_recovery_ui.operations[&id].request,request);
        assert!(app.project_recovery_ui.operations[&id].in_flight);
        drop(app.update_project_recovery(Action::OperationFinished(id,uuid::Uuid::new_v4(),Err("unrelated reply".into()))));
        assert!(app.project_recovery_ui.operations[&id].in_flight);
        let mut replacement=request.clone();replacement.operation=uuid::Uuid::new_v4();
        drop(app.submit_project_removal_operation(id,replacement));
        assert_eq!(app.project_recovery_ui.operations[&id].request,request);
        drop(app.update_project_recovery(Action::OperationFinished(id,request.operation,Err("reply lost".into()))));
        assert_eq!(app.project_recovery_ui.operations[&id].request,request);
        assert!(!app.project_recovery_ui.operations[&id].in_flight);
        app.workspace_model.identity=uuid::Uuid::new_v4();
        drop(app.update_project_recovery(Action::Command(id,ProjectRecoveryAction::RetryOperation(request.operation))));
        assert!(!app.project_recovery_ui.operations[&id].in_flight,"a registry replacement cannot refresh the original retry's authority");
        assert!(app.workspace_model.project(&expected.repository).is_some());
    }

    #[tokio::test]
    async fn locate_keeps_only_its_original_window_recovery_until_exact_resolution() {
        let fixture=tempfile::tempdir().unwrap();
        let old=fixture.path().join("original");let moved=fixture.path().join("moved");
        let other_root=fixture.path().join("other");
        std::fs::create_dir(&old).unwrap();std::fs::create_dir(&other_root).unwrap();
        let (mut app,project)=app(&old);
        let other=ProjectId::new();
        app.sidebar_model.projects.push(SidebarProject::new(other,RepositoryId::new(),"Other".into(),other_root.clone(),Vec::new(),Vec::new()).with_kind(ProjectKind::Folder));
        let authority=crate::editor::RecoverySlot::reserve().unwrap().retain("original pinned file operation".into(),||Ok(()));
        let failure=crate::editor_workspace_edit::PublicationFailure {message:"original cleanup still owns files".into(),recovery:Some(authority.clone()),worktree:Some(old.clone())};
        drop(app.update(Message::EditorFeatures(super::super::editor_features::Action::Published{ticket:999,result:Err(failure)})));
        // Exercise actual chooser intake; dropping its Task avoids opening a
        // native dialog. The capture precedes any asynchronous location work.
        let expected=app.project_instance(project).unwrap();
        drop(app.update_project_recovery(Action::Command(project,ProjectRecoveryAction::Locate)));
        let generation=app.project_recovery_ui.location_generations[&project];
        assert!(app.project_retains_relocated_recovery(project));
        std::fs::rename(&old,&moved).unwrap();std::fs::create_dir(&old).unwrap();
        let later=crate::editor::RecoverySlot::reserve().unwrap().retain("later operation at the reused old name".into(),||Ok(()));
        let failure=crate::editor_workspace_edit::PublicationFailure {message:"different retained operation".into(),recovery:Some(later.clone()),worktree:Some(old.clone())};
        drop(app.update(Message::EditorFeatures(super::super::editor_features::Action::Published{ticket:1000,result:Err(failure)})));
        let relocation=Relocation {repository:old.clone(),destination:moved.clone(),old_root:old.clone(),new_root:moved.clone()};
        // Local Applied uses its original intake, not a fresh old-path query.
        let mut model=app.workspace_model.clone();model.sequence+=1;
        model.projects[0].repository=moved.clone();
        drop(app.update_project_recovery(Action::Applied(project,expected,generation,relocation.clone(),Ok(model))));
        assert_eq!(app.project_root(app.sidebar_model.project(project).unwrap()),moved);
        assert!(app.project_has_editor_recovery(project));assert!(!app.project_has_editor_recovery(other));
        assert!(app.project_worktree_publication_recovery(project,&relocation.new_root));
        assert!(!app.project_worktree_publication_recovery(other,&other_root));
        assert_eq!(app.project_recovery_ui.relocated_recovery[&project],authority);
        let worker=authority.clone();tokio::task::spawn_blocking(move||worker.retry()).await.unwrap().unwrap();
        assert!(authority.is_resolved());assert!(!later.is_resolved());
        assert!(!app.project_has_publication_recovery(project),"later old-name recovery cannot inherit the original project's marker");
        assert!(!app.project_worktree_publication_recovery(project,&relocation.new_root));
        assert!(app.publication_recovery_blocks(&old));
        // A project created after Locate has no marker, even if its repository
        // spelling matches the moved display root. No authority is transferred.
        let replacement=ProjectId::new();
        app.sidebar_model.projects.push(SidebarProject::new(replacement,RepositoryId::new(),"Replacement".into(),relocation.new_root,Vec::new(),Vec::new()).with_kind(ProjectKind::Folder));
        assert!(!app.project_retains_relocated_recovery(replacement));
        let worker=later.clone();tokio::task::spawn_blocking(move||worker.retry()).await.unwrap().unwrap();
    }

    fn publication_card(app: &mut Cagents, project: ProjectId, root: &Path) -> CardId {
        let original="fn old() {}\n";
        for name in ["participant.rs","unopened.rs"] {std::fs::write(root.join(name),original).unwrap();}
        app.settings.language_servers.clear();app.lsp=LspManager::new(Vec::new());
        drop(app.add_terminal(root.to_owned()));let card=app.durable.cards.last().unwrap().id;
        let (mut runtime,_)=RepositoryRuntime::open_fixture(card,"participant.rs",original);
        runtime.descriptor.project=project;runtime.descriptor.repository=root.to_owned();runtime.descriptor.worktree=root.to_owned();
        app.card_state_mut(card).unwrap().kind=CardKind::Repository(runtime.descriptor.clone());
        app.cards.insert(card,CardRuntime::Repository(Box::new(runtime)));card
    }

    #[tokio::test]
    async fn held_publisher_recovery_keeps_original_project_before_or_after_locate_applies() {
        use cosmic::iced::futures::StreamExt;
        for complete_before_move in [false,true] {
            let fixture=tempfile::tempdir().unwrap();let old=fixture.path().join("original");let moved=fixture.path().join("moved");
            std::fs::create_dir(&old).unwrap();let (mut app,project)=app(&old);
            let card=publication_card(&mut app,project,&old);
            let publishing=app.project_editor_publication_fixture(card,&old,543);
            assert!(app.card_has_project_editor_write(card));
            let expected=app.project_instance(project).unwrap();
            drop(app.update_project_recovery(Action::Command(project,ProjectRecoveryAction::Locate)));
            let generation=app.project_recovery_ui.location_generations[&project];
            assert!(app.project_recovery_ui.relocation_publications[&543].contains(&project));
            // Collect the actual held publisher's result. Recovery is a typed
            // fixture input below; this test does not simulate FS compensation.
            let mut stream=iced::runtime::task::into_stream(publishing).unwrap();let mut received=false;
            while let Some(action)=stream.next().await {
                if matches!(action,iced::runtime::Action::Output(cosmic::Action::App(Message::EditorFeatures(super::super::editor_features::Action::Published{ticket:543,..})))) {received=true;break;}
            }
            assert!(received);
            // A stale/no-runtime receipt must still attribute the original
            // project via captured ticket, never via current paths or cards.
            app.cards.remove(&card);
            let authority=crate::editor::RecoverySlot::reserve().unwrap().retain("late exact publisher recovery".into(),||Ok(()));
            let result=Err(crate::editor_workspace_edit::PublicationFailure{message:"late owned cleanup".into(),recovery:Some(authority.clone()),worktree:Some(old.clone())});
            if complete_before_move {
                drop(app.update(Message::EditorFeatures(super::super::editor_features::Action::Published{ticket:543,result:result.clone()})));
            }
            std::fs::rename(&old,&moved).unwrap();std::fs::create_dir(&old).unwrap();
            let relocation=Relocation{repository:old.clone(),destination:moved.clone(),old_root:old.clone(),new_root:moved.clone()};
            let mut model=app.workspace_model.clone();model.sequence+=1;model.projects[0].repository=moved.clone();
            drop(app.update_project_recovery(Action::Applied(project,expected,generation,relocation,Ok(model))));
            if !complete_before_move {
                drop(app.update(Message::EditorFeatures(super::super::editor_features::Action::Published{ticket:543,result})));
            }
            assert!(!app.project_recovery_ui.relocation_publications.contains_key(&543));
            assert_eq!(app.project_recovery_ui.relocated_recovery[&project],authority);
            assert!(app.project_has_publication_recovery(project));assert!(!app.cards.contains_key(&card));
            let worker=authority.clone();tokio::task::spawn_blocking(move||worker.retry()).await.unwrap().unwrap();
            assert!(!app.project_has_publication_recovery(project));
        }
    }

    #[tokio::test]
    async fn new_publication_waits_for_its_own_locate_but_unrelated_project_can_publish() {
        use cosmic::iced::futures::StreamExt;
        let fixture=tempfile::tempdir().unwrap();let root=fixture.path().join("locating");let other_root=fixture.path().join("other");
        std::fs::create_dir(&root).unwrap();std::fs::create_dir(&other_root).unwrap();
        let (mut app,project)=app(&root);let card=publication_card(&mut app,project,&root);
        let other=ProjectId::new();app.sidebar_model.projects.push(SidebarProject::new(other,RepositoryId::new(),"Other".into(),other_root.clone(),Vec::new(),Vec::new()).with_kind(ProjectKind::Folder));
        let other_card=publication_card(&mut app,other,&other_root);
        drop(app.update_project_recovery(Action::Command(project,ProjectRecoveryAction::Locate)));
        assert!(app.project_relocation_blocks_publication(&HashSet::from([card])));
        assert!(!app.project_relocation_blocks_publication(&HashSet::from([other_card])));
        drop(app.project_editor_publication_fixture(card,&root,544));
        assert!(!app.editor_features.publishing());assert!(!app.card_has_project_editor_write(card));
        assert_eq!(std::fs::read_to_string(root.join("unopened.rs")).unwrap(),"fn old() {}\n");
        let task=app.project_editor_publication_fixture(other_card,&other_root,545);
        assert!(app.editor_features.publishing());assert!(app.card_has_project_editor_write(other_card));
        let mut stream=iced::runtime::task::into_stream(task).unwrap();let mut delivered=false;
        while let Some(action)=stream.next().await {
            if let iced::runtime::Action::Output(cosmic::Action::App(message @ Message::EditorFeatures(super::super::editor_features::Action::Published{ticket:545,..})))=action {delivered=true;drop(app.update(message));}
        }
        assert!(delivered);assert!(!app.card_has_project_editor_write(other_card));
        assert_eq!(std::fs::read_to_string(other_root.join("unopened.rs")).unwrap(),"fn renamed() {}\n");
    }

    #[test]
    fn reconnect_follows_non_reused_project_identity_even_when_old_path_is_reopened() {
        let old = Path::new("/old/project");
        let (mut app, id) = app(old);
        let identity = uuid::Uuid::new_v4();
        let durable = Arc::make_mut(&mut app.durable);
        durable.workspace_model_identity = Some(identity);
        durable.shared_project_bindings.insert(
            id,
            crate::model::SharedProjectBinding {
                ordinal: 7,
                root: Some(old.into()),
            },
        );
        app.workspace_model = crate::workspace_model::WorkspaceModel {
            identity,
            projects: vec![
                crate::workspace_model::ModelProject {
                    checkout: None,
                    primary_checkout: None,
                    repository: "/moved/project".into(),
                    name: "Original".into(),
                    kind: ProjectKind::Folder,
                    ordinal: 7,
                },
                crate::workspace_model::ModelProject {
                    checkout: None,
                    primary_checkout: None,
                    repository: old.into(),
                    name: "Replacement".into(),
                    kind: ProjectKind::Folder,
                    ordinal: 8,
                },
            ],
            ..Default::default()
        };
        drop(app.recover_shared_project_relocations());
        assert_eq!(
            app.sidebar_model.project(id).unwrap().repository,
            Path::new("/moved/project")
        );
        assert_eq!(app.durable.shared_project_bindings[&id].ordinal, 7);
        // A fresh project at the reused path is not associated with rank7.
        let fresh = ProjectId::new();
        app.sidebar_model.projects.push(
            SidebarProject::new(
                fresh,
                RepositoryId::new(),
                "Replacement".into(),
                old.into(),
                Vec::new(),
                Vec::new(),
            )
            .with_kind(ProjectKind::Folder),
        );
        Arc::make_mut(&mut app.durable)
            .shared_project_bindings
            .insert(
                fresh,
                crate::model::SharedProjectBinding {
                    ordinal: 8,
                    root: Some(old.into()),
                },
            );
        drop(app.recover_shared_project_relocations());
        assert_eq!(app.sidebar_model.project(fresh).unwrap().repository, old);
    }

    #[test]
    fn reconnect_follows_primary_move_with_stable_external_metadata_and_keeps_linked_checkout() {
        let metadata = Path::new("/metadata/project.git");
        let old = PathBuf::from("/old/primary");
        let new = PathBuf::from("/new/primary");
        let linked = PathBuf::from("/other/linked");
        let (mut app, id) = app(metadata);
        app.sidebar_model
            .projects
            .iter_mut()
            .find(|project| project.id == id)
            .unwrap()
            .kind = ProjectKind::Git;
        let identity = uuid::Uuid::new_v4();
        let durable = Arc::make_mut(&mut app.durable);
        durable.workspace_model_identity = Some(identity);
        durable.shared_project_bindings.insert(
            id,
            crate::model::SharedProjectBinding {
                ordinal: 7,
                root: Some(old.clone()),
            },
        );
        app.workspace_model = crate::workspace_model::WorkspaceModel {
            identity,
            projects: vec![crate::workspace_model::ModelProject {
                repository: metadata.into(),
                name: "Project".into(),
                kind: ProjectKind::Git,
                ordinal: 7,
                checkout: Some(linked.clone()),
                primary_checkout: Some(new.clone()),
            }],
            ..Default::default()
        };
        app.project_ui
            .checkout_roots
            .insert(metadata.into(), linked.clone());
        app.project_ui
            .primary_checkout_roots
            .insert(metadata.into(), old.clone());
        drop(app.recover_shared_project_relocations());
        assert_eq!(app.sidebar_model.project(id).unwrap().repository, metadata);
        assert_eq!(
            app.project_root(app.sidebar_model.project(id).unwrap()),
            new
        );
        assert_eq!(app.project_ui.checkout_roots[metadata], linked);
        assert_eq!(app.project_ui.primary_checkout_roots[metadata], new);
        // Merely selecting another linked checkout cannot move the primary.
        app.workspace_model.projects[0].checkout = Some("/another/linked".into());
        app.workspace_model.projects[0].primary_checkout = None;
        drop(app.recover_shared_project_relocations());
        assert_eq!(
            app.project_root(app.sidebar_model.project(id).unwrap()),
            new
        );
    }

    #[test]
    fn reconnect_does_not_interpret_a_replacement_registry_rank_as_the_old_project() {
        let old = Path::new("/old/project");
        let (mut app, id) = app(old);
        let durable = Arc::make_mut(&mut app.durable);
        durable.workspace_model_identity = Some(uuid::Uuid::new_v4());
        durable.shared_project_bindings.insert(
            id,
            crate::model::SharedProjectBinding {
                ordinal: 7,
                root: Some(old.into()),
            },
        );
        app.workspace_model = crate::workspace_model::WorkspaceModel {
            identity: uuid::Uuid::new_v4(),
            projects: vec![crate::workspace_model::ModelProject {
                checkout: None,
                primary_checkout: None,
                repository: "/unrelated/project".into(),
                name: "Other".into(),
                kind: ProjectKind::Folder,
                ordinal: 7,
            }],
            ..Default::default()
        };
        drop(app.recover_shared_project_relocations());
        assert_eq!(app.sidebar_model.project(id).unwrap().repository, old);
    }

    #[test]
    fn locate_external_git_directory_repairs_only_its_saved_checkout() {
        let fixture = tempfile::tempdir().unwrap();
        let old = fixture.path().join("old checkout");
        let metadata = fixture.path().join("external.git");
        let new = fixture.path().join("new checkout");
        let git = |args: &[&std::ffi::OsStr]| {
            let output = std::process::Command::new("git")
                .current_dir(fixture.path())
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            output
        };
        git(&[
            "init".as_ref(),
            "--separate-git-dir".as_ref(),
            metadata.as_os_str(),
            old.as_os_str(),
        ]);
        // Relative values are relative to the Git directory, including `..`.
        git(&[
            "--git-dir".as_ref(),
            metadata.as_os_str(),
            "config".as_ref(),
            "core.worktree".as_ref(),
            "../old checkout".as_ref(),
        ]);
        let (mut app, id) = app(&metadata);
        app.sidebar_model
            .projects
            .iter_mut()
            .find(|project| project.id == id)
            .unwrap()
            .kind = ProjectKind::Git;
        Arc::make_mut(&mut app.durable)
            .shared_project_bindings
            .insert(
                id,
                crate::model::SharedProjectBinding {
                    ordinal: 1,
                    root: Some(old.clone()),
                },
            );
        std::fs::rename(&old, &new).unwrap();
        assert_eq!(
            app.project_root(app.sidebar_model.project(id).unwrap()),
            old
        );
        assert!(project_recovery::root_missing(&old));
        let generation=uuid::Uuid::new_v4();
        app.project_recovery_ui.location_generations.insert(id,generation);
        let message = super::super::tests::task_app_message(
            app.update_project_recovery(Action::Located(id, app.project_instance(id).unwrap(), old.clone(), generation, Ok(new.clone()))),
        );
        let Message::ProjectRecovery(Action::Prepared(_, _, _, result)) = message else {
            panic!()
        };
        let relocation = result.unwrap();
        assert_eq!(relocation.repository, metadata);
        assert_eq!(relocation.destination, metadata);
        assert!(!relocation.verification_only());
        let unchanged=git(&["--git-dir".as_ref(),metadata.as_os_str(),"config".as_ref(),"--get".as_ref(),"core.worktree".as_ref()]);
        assert_eq!(String::from_utf8(unchanged.stdout).unwrap().trim(),"../old checkout","Chooser preparation must not mutate Git configuration before host instance admission");
        crate::project_recovery::git_repair::repair(&relocation,&[]).unwrap();
        let output = std::process::Command::new("git")
            .current_dir(&new)
            .args(["rev-parse", "--show-toplevel"])
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8(output.stdout).unwrap().trim(),
            new.to_str().unwrap()
        );
        git(&[
            "--git-dir".as_ref(),
            metadata.as_os_str(),
            "config".as_ref(),
            "core.worktree".as_ref(),
            "/unrelated/checkout".as_ref(),
        ]);
        assert!(
            crate::project_recovery::git_repair::repair_checkout_config(&relocation)
                .unwrap_err()
                .contains("another checkout")
        );
        let output = git(&[
            "--git-dir".as_ref(),
            metadata.as_os_str(),
            "config".as_ref(),
            "--get".as_ref(),
            "core.worktree".as_ref(),
        ]);
        assert_eq!(
            String::from_utf8(output.stdout).unwrap().trim(),
            "/unrelated/checkout"
        );
    }

    #[test]
    fn locating_moved_checkout_repairs_linked_worktree_paths() {
        let parent = tempfile::tempdir().unwrap();
        let old = parent.path().join("original");
        std::fs::create_dir(&old).unwrap();
        let git = |path: &Path, args: &[&str]| {
            let output = std::process::Command::new("git")
                .current_dir(path)
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&old, &["init", "-b", "main"]);
        git(
            &old,
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "--allow-empty",
                "-m",
                "fixture",
            ],
        );
        let linked = old.join("linked");
        git(
            &old,
            &["worktree", "add", "-b", "linked", linked.to_str().unwrap()],
        );
        let (mut app, id) = app(&old.join(".git"));
        let project = app
            .sidebar_model
            .projects
            .iter_mut()
            .find(|project| project.id == id)
            .unwrap();
        project.kind = ProjectKind::Git;
        project.worktrees.push(SidebarWorktree::from_durable(
            crate::model::WorktreeId::new(),
            linked,
            Some("linked".into()),
        ));
        let located = parent.path().join("moved");
        std::fs::rename(&old, &located).unwrap();
        let relocation = Relocation {
            repository: old.join(".git"),
            destination: located.join(".git"),
            old_root: old.clone(),
            new_root: located.clone(),
        };
        assert!(crate::project_recovery::git_repair::belongs_to_relocated_worktree(
            &located.join("linked"),
            &relocation
        ));
        let generation=uuid::Uuid::new_v4();
        app.project_recovery_ui.location_generations.insert(id,generation);
        // The fixture's Git primary is the parent of its metadata directory.
        Arc::make_mut(&mut app.durable).shared_project_bindings.get_mut(&id).unwrap().root=Some(old.clone());
        let task = app.update_project_recovery(Action::Located(id, app.project_instance(id).unwrap(), old.clone(), generation, Ok(located.clone())));
        let result = super::super::tests::task_app_message(task);
        let Message::ProjectRecovery(Action::Prepared(_, _, _, result)) = result else {
            panic!()
        };
        let relocation = result.unwrap();
        assert_eq!(relocation.destination, located.join(".git"));
        crate::project_recovery::git_repair::repair(&relocation,&[old.join("linked")]).unwrap();
        git(&located.join("linked"), &["status", "--porcelain"]);
    }

    #[test]
    fn locate_does_not_repair_an_unrelated_worktree_with_a_matching_admin_name() {
        let fixture = tempfile::tempdir().unwrap();
        let old = fixture.path().join("old");
        let new = fixture.path().join("new");
        let linked = new.join("linked");
        let admin = new.join(".git/worktrees/linked");
        std::fs::create_dir_all(&admin).unwrap();
        std::fs::create_dir(&linked).unwrap();
        std::fs::write(
            linked.join(".git"),
            format!("gitdir: {}\n", old.join(".git/worktrees/linked").display()),
        )
        .unwrap();
        std::fs::write(
            admin.join("gitdir"),
            fixture.path().join("unrelated/.git").display().to_string(),
        )
        .unwrap();
        let relocation = Relocation {
            repository: old.join(".git"),
            destination: new.join(".git"),
            old_root: old,
            new_root: new,
        };
        assert!(!crate::project_recovery::git_repair::belongs_to_relocated_worktree(&linked, &relocation));
    }

    #[test]
    fn delayed_old_root_probe_cannot_override_same_key_locate() {
        for direct in [false,true] {
            let metadata=Path::new("/metadata/project.git");
            let old=PathBuf::from("/old/primary");
            let new=PathBuf::from("/new/primary");
            let (mut app,id)=app(metadata);
            app.sidebar_model.projects.iter_mut().find(|project|project.id==id).unwrap().kind=ProjectKind::Git;
            Arc::make_mut(&mut app.durable).shared_project_bindings.insert(id,crate::model::SharedProjectBinding {ordinal:7,root:Some(old.clone())});
            let relocation=Relocation {repository:metadata.into(),destination:metadata.into(),old_root:old.clone(),new_root:new.clone()};
            let _=app.apply_project_relocation(&relocation);
            let token=CancellationToken::default();
            app.refresh_ui.projects.insert(id,token.clone());
            let response=if direct {Action::Checked(vec![(id,metadata.into(),old,Some("old root disappeared".into()))])}
                else {Action::AvailabilityChecked(vec![(id,metadata.into(),old,Some("old root disappeared".into()),true)])};
            let _=app.update_project_recovery(response);
            let project=app.sidebar_model.project(id).unwrap();
            assert_eq!(app.project_root(project),new);
            assert!(!project.missing);
            assert!(project.location_repair.is_none());
            assert!(!token.is_cancelled());
        }
    }

    #[test]
    fn missing_primary_keeps_verified_linked_checkout_available_and_retains_repair() {
        let parent=tempfile::tempdir().unwrap();
        let primary=parent.path().join("primary");
        let metadata=parent.path().join("metadata.git");
        let linked=parent.path().join("linked");
        let foreign=parent.path().join("foreign");
        let git=|directory:&Path,args:&[&std::ffi::OsStr]| {
            let result=std::process::Command::new("git").current_dir(directory)
                .args(["-c","core.fsmonitor=false","-c","commit.gpgsign=false","-c","core.hooksPath=/dev/null"]).args(args)
                .env("GIT_CONFIG_GLOBAL","/dev/null").env("GIT_CONFIG_NOSYSTEM","1")
                .env_remove("GIT_DIR").env_remove("GIT_COMMON_DIR").env_remove("GIT_WORK_TREE").env_remove("GIT_INDEX_FILE").output().unwrap();
            assert!(result.status.success(),"{}",String::from_utf8_lossy(&result.stderr));
        };
        git(parent.path(),&["init".as_ref(),"--separate-git-dir".as_ref(),metadata.as_os_str(),primary.as_os_str()]);
        git(&primary,&["-c".as_ref(),"user.name=Fixture".as_ref(),"-c".as_ref(),"user.email=fixture@example.invalid".as_ref(),"commit".as_ref(),"--allow-empty".as_ref(),"-m".as_ref(),"fixture".as_ref()]);
        git(&primary,&["worktree".as_ref(),"add".as_ref(),"-b".as_ref(),"linked".as_ref(),linked.as_os_str()]);
        git(parent.path(),&["init".as_ref(),foreign.as_os_str()]);
        let (mut app,id)=app(&metadata);
        let project=app.sidebar_model.projects.iter_mut().find(|project|project.id==id).unwrap();
        project.kind=ProjectKind::Git;
        project.unreachable=Some("primary checkout disappeared during its previous Git read".into());
        app.project_ui.checkout_roots.insert(metadata.clone(),linked.clone());
        app.project_ui.primary_checkout_roots.insert(metadata.clone(),primary.clone());
        Arc::make_mut(&mut app.durable).shared_project_bindings.insert(id,crate::model::SharedProjectBinding {ordinal:7,root:Some(primary.clone())});
        let cancellation=CancellationToken::default();
        app.refresh_ui.projects.insert(id,cancellation.clone());
        let tick=|app:&mut Cagents| {
            let message=super::super::tests::task_app_message(app.update_project_recovery(Action::Tick));
            let Message::ProjectRecovery(action)=message else {panic!()};
            let _=app.update_project_recovery(action);
        };
        std::fs::remove_dir_all(&primary).unwrap();
        tick(&mut app);
        let project=app.sidebar_model.project(id).unwrap();
        assert!(!project.missing,"the verified same-repository linked checkout remains available");
        assert!(project.location_repair.as_deref().unwrap().contains(primary.to_str().unwrap()));
        assert!(project.unreachable.is_none(),"repair notice must not replace working navigation");
        assert_eq!(app.project_root(project),primary,"repair retains original root association");
        assert!(!cancellation.is_cancelled(),"valid linked checkout keeps its in-flight Git read");
        assert_eq!(app.project_ui.checkout_roots[&metadata],linked);
        git(&linked,&["status".as_ref(),"--porcelain".as_ref()]);
        // An unrelated valid repository is not an availability fallback.
        std::fs::remove_dir_all(&linked).unwrap();
        app.project_ui.checkout_roots.insert(metadata,foreign);
        tick(&mut app);
        let project=app.sidebar_model.project(id).unwrap();
        assert!(project.missing);
        assert!(project.location_repair.is_none());
        assert!(cancellation.is_cancelled(),"all known same-project checkouts are unavailable");
    }

    #[test]
    fn disappearing_root_marks_row_missing_and_returning_root_recovers() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("project");
        std::fs::create_dir(&root).unwrap();
        let (mut app, id) = app(&root);
        drop(app.update_project_recovery(Action::Checked(vec![(id, root.clone(), root.clone(), None)])));
        assert!(!app.project_missing(id));
        std::fs::remove_dir(&root).unwrap();
        drop(app.update_project_recovery(Action::Checked(vec![(
            id,
            root.clone(),
            root.clone(),
            project_recovery::missing_reason(&root),
        )])));
        assert!(app.project_missing(id));
        assert!(app.sidebar_model.project(id).unwrap().unreachable.is_some());
        std::fs::create_dir(&root).unwrap();
        drop(app.update_project_recovery(Action::Checked(vec![(
            id,
            root.clone(),
            root.clone(),
            project_recovery::missing_reason(&root),
        )])));
        assert!(!app.project_missing(id));
    }
    #[test]
    fn session_location_warning_stays_with_exact_pane_and_clears_after_verified_model() {
        let (mut app, _) = app(Path::new("/old/project"));
        drop(app.add_terminal("/old/project".into()));
        let card = app.durable.cards.last().unwrap().id;
        let CardKind::Terminal(session) = &app.card_state(card).unwrap().kind else {
            panic!()
        };
        let session = session.id;
        app.workspace_model.session_location_refusals.push(
            crate::workspace_model::SessionLocationRefusal {
                session,
                directory: "/new/project".into(),
                reason: "The owned provider has not confirmed this directory; retry when ready."
                    .into(),
            },
        );
        assert_eq!(app.card_location_refusal(card).unwrap().session, session);
        assert!(app.project_location_notice(card).is_some());
        drop(app.update_project_recovery(Action::SessionDetails(card)));
        assert!(app.project_recovery_ui.details.contains(&card));
        app.workspace_model.session_location_refusals.clear();
        assert!(app.card_location_refusal(card).is_none());
        assert!(app.project_location_notice(card).is_none());
    }

    #[test]
    fn locate_preserves_live_terminal_launch_provenance_and_observed_directory() {
        let old = Path::new("/old/project");
        let (mut app, _) = app(old);
        drop(app.add_terminal(old.into()));
        let card = app.durable.cards.last().unwrap().id;
        let CardKind::Terminal(session) = &mut app.card_state_mut(card).unwrap().kind else {
            panic!()
        };
        session.continuity = TerminalContinuity::Live {
            host_session_id: HostSessionId::new("fixture"),
            boot_id: crate::model::BootId::new("fixture"),
            host_incarnation: HostIncarnation::new("fixture"),
        };
        let original = app.card_state(card).unwrap().kind.clone();
        Arc::make_mut(&mut app.durable)
            .card_scopes
            .insert(card, old.into());
        app.terminal_working_directories.insert(card, old.into());
        let relocation = Relocation {
            repository: old.into(),
            destination: "/new/project".into(),
            old_root: old.into(),
            new_root: "/new/project".into(),
        };
        drop(app.apply_project_relocation(&relocation));
        assert_eq!(
            app.card_state(card).unwrap().kind,
            original,
            "logical placement must not rewrite immutable process launch provenance"
        );
        assert_eq!(app.durable.card_scopes[&card], relocation.new_root);
        assert_eq!(app.terminal_working_directories[&card], old);
    }

    #[test]
    fn missing_parent_does_not_claim_independently_open_nested_project() {
        let old = Path::new("/old/project");
        let (mut app, id) = app(old);
        let nested = ProjectId::new();
        app.sidebar_model.projects.push(
            SidebarProject::new(
                nested,
                RepositoryId::new(),
                "Nested".into(),
                old.join("nested"),
                Vec::new(),
                Vec::new(),
            )
            .with_kind(ProjectKind::Folder),
        );
        let card = CardId::new();
        Arc::make_mut(&mut app.durable).cards.push(CardState {
            id: card,
            generation: 0,
            title: "Nested".into(),
            width_fraction: 0.5,
            existence: Existence::Open,
            exposure: Exposure::Offscreen,
            residency: Residency::Cold,
            kind: CardKind::Repository(RepositoryView {
                project: nested,
                repository: old.join("nested"),
                worktree: old.join("nested"),
                selected_file: None,
            }),
        });
        Arc::make_mut(&mut app.durable)
            .card_scopes
            .insert(card, old.join("nested"));
        app.sidebar_model
            .projects
            .iter_mut()
            .find(|project| project.id == id)
            .unwrap()
            .missing = true;
        assert!(!app.missing_project_cards().contains(&card));
        let relocation = Relocation {
            repository: old.into(),
            destination: "/new/project".into(),
            old_root: old.into(),
            new_root: "/new/project".into(),
        };
        drop(app.apply_project_relocation(&relocation));
        assert_eq!(app.durable.card_scopes[&card], old.join("nested"));
        let CardKind::Repository(view) = &app.card_state(card).unwrap().kind else {
            panic!()
        };
        assert_eq!(view.worktree, old.join("nested"));
    }

    #[test]
    fn locate_repoints_durable_project_and_cards_then_schedules_git_refresh() {
        let old = Path::new("/old/project");
        let (mut app, id) = app(old);
        let view = RepositoryView {
            project: id,
            repository: old.into(),
            worktree: old.join("nested"),
            selected_file: Some("file.rs".into()),
        };
        let card = CardId::new();
        Arc::make_mut(&mut app.durable).cards.push(CardState {
            id: card,
            generation: 0,
            title: "File".into(),
            width_fraction: 0.5,
            existence: Existence::Open,
            exposure: Exposure::Offscreen,
            residency: Residency::Cold,
            kind: CardKind::Repository(view),
        });
        let relocation = Relocation {
            repository: old.into(),
            destination: "/new/project".into(),
            old_root: old.into(),
            new_root: "/new/project".into(),
        };
        drop(app.apply_project_relocation(&relocation));
        assert_eq!(
            app.sidebar_model.project(id).unwrap().repository,
            relocation.destination
        );
        let CardKind::Repository(view) = &app.card_state(card).unwrap().kind else {
            panic!()
        };
        assert_eq!(view.worktree, Path::new("/new/project/nested"));
        assert!(
            app.refresh_ui.projects.contains_key(&id),
            "locate schedules a fresh project/Git load"
        );
    }
    #[test]
    fn stale_locate_chooser_and_preparation_preserve_reopened_project_and_newer_intent() {
        let directory=tempfile::tempdir().unwrap();
        let old=directory.path().join("old");std::fs::create_dir(&old).unwrap();
        let selected=directory.path().join("selected");std::fs::create_dir(&selected).unwrap();
        let (mut app,id)=app(&old);
        let expected=app.project_instance(id).unwrap();
        let generation=uuid::Uuid::new_v4();
        app.project_recovery_ui.locating.insert(id);
        app.project_recovery_ui.location_generations.insert(id,generation);
        // A fresh project occupies the same persisted key and local ID.
        app.workspace_model.projects[0].ordinal+=1;
        Arc::make_mut(&mut app.durable).shared_project_bindings.get_mut(&id).unwrap().ordinal+=1;
        let before=app.workspace_model.clone();
        drop(app.update_project_recovery(Action::Located(id,expected.clone(),old.clone(),generation,Ok(selected.clone()))));
        assert_eq!(app.workspace_model,before);
        assert!(!app.project_recovery_ui.locating.contains(&id));
        let newer=uuid::Uuid::new_v4();
        app.project_recovery_ui.locating.insert(id);
        app.project_recovery_ui.location_generations.insert(id,newer);
        let relocation=Relocation{repository:old.clone(),destination:selected.clone(),old_root:old,new_root:selected};
        drop(app.update_project_recovery(Action::Prepared(id,expected,generation,Ok(relocation))));
        assert_eq!(app.project_recovery_ui.location_generations.get(&id),Some(&newer));
        assert!(app.project_recovery_ui.locating.contains(&id));
        assert_eq!(app.workspace_model,before);
    }

    #[test]
    fn stale_locate_from_recreated_registry_never_uses_same_rank_replacement() {
        let directory=tempfile::tempdir().unwrap();
        let (mut app,id)=app(directory.path());
        let expected=app.project_instance(id).unwrap();
        let generation=uuid::Uuid::new_v4();
        app.project_recovery_ui.locating.insert(id);
        app.project_recovery_ui.location_generations.insert(id,generation);
        let replacement=uuid::Uuid::new_v4();
        app.workspace_model.identity=replacement;
        Arc::make_mut(&mut app.durable).workspace_model_identity=Some(replacement);
        let before=app.workspace_model.clone();
        drop(app.update_project_recovery(Action::Located(id,expected,directory.path().to_owned(),generation,Ok(directory.path().to_owned()))));
        assert_eq!(app.workspace_model,before);
        assert_eq!(app.project_instance(id).unwrap().registry,replacement);
        assert!(!app.project_recovery_ui.locating.contains(&id));
    }

}
