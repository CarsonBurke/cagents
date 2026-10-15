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
    locating: HashSet<ProjectId>,
    location_generations: HashMap<ProjectId,uuid::Uuid>,
    pub(super) closing_panes: bool,
    details: HashSet<CardId>,
}
#[derive(Debug, Clone)]
pub enum Action {
    Tick,
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
    fn project_cards(&self, id: ProjectId) -> HashSet<CardId> {
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
    pub(super) fn project_has_editor_recovery(&self, project: ProjectId) -> bool {
        self.project_cards(project).into_iter().any(|card| self.card_has_editor_recovery(card))
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
                let edit = crate::workspace_model::ModelEdit::RelocateProject {
                    project: expected.clone(),
                    relocation: relocation.clone(),
                };
                cosmic::task::future(async move {
                    let result = tokio::task::spawn_blocking(move || {
                        let socket = crate::terminal::ensure_daemon_ready()
                            .map_err(|error| error.to_string())?;
                        crate::control::ControlClient::connect(&socket)
                            .map_err(|error| error.to_string())?
                            .edit_workspace_model(edit)
                            .map_err(|error| error.to_string())
                    })
                    .await
                    .map_err(|error| error.to_string())
                    .and_then(|result| result);
                    Message::ProjectRecovery(Action::Applied(id, expected, generation, relocation, result))
                })
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
                            return self.apply_project_relocation(&relocation);
                        }
                        self.workspace_model = model;
                        let relocated = if relocation.verification_only() {
                            Task::none()
                        } else {
                            self.apply_project_relocation(&relocation)
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
