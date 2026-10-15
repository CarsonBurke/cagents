//! Root availability is independent of Git. Missing projects use a cheap
//! filesystem probe until restored; no Git/GitHub polling is scheduled for them.
use super::*;
use crate::{project_recovery::{self, Relocation}, sidebar::ProjectRecoveryAction};

#[derive(Default)]
pub(super) struct State { checking: bool, locating: HashSet<ProjectId>, pub(super) closing_panes: bool, details: HashSet<CardId> }
#[derive(Debug, Clone)]
pub enum Action {
    Tick,
    Checked(Vec<(ProjectId, PathBuf, Option<String>)>),
    Command(ProjectId, ProjectRecoveryAction),
    VerifyProject(ProjectId),
    SessionDetails(CardId),
    Located(ProjectId, Result<PathBuf, String>),
    Prepared(ProjectId, Result<Relocation, String>),
    Applied(ProjectId, Relocation, Result<crate::workspace_model::WorkspaceModel, String>),
}

fn root(project: &SidebarProject) -> PathBuf {
    if project.kind == ProjectKind::Git && project.repository.file_name().is_some_and(|name| name == ".git") {
        project.repository.parent().unwrap_or(&project.repository).to_path_buf()
    } else { project.repository.clone() }
}

// A Locate choice must not attach unrelated linked checkouts to this repo.
// Verify both the old marker and the moved admin directory's backlink before
// handing an explicit linked path to `git worktree repair`.
fn belongs_to_relocated_worktree(path: &Path, relocation: &Relocation) -> bool {
    let marker = match std::fs::read_to_string(path.join(".git")) { Ok(marker) => marker, Err(_) => return false };
    let Some(admin) = marker.trim().strip_prefix("gitdir: ") else { return false; };
    let admin = Path::new(admin);
    let admin = if admin.is_absolute() { admin.to_path_buf() } else { path.join(admin) };
    if !admin.starts_with(&relocation.repository) { return false; }
    let admin = relocation.mapped(&admin);
    let backlink = match std::fs::read_to_string(admin.join("gitdir")) { Ok(backlink) => backlink, Err(_) => return false };
    relocation.mapped(Path::new(backlink.trim())) == path.join(".git")
}

// `git worktree repair` does not update an external Git directory's
// core.worktree. Change only the exact previous checkout chosen by Locate.
fn repair_checkout_config(relocation: &Relocation) -> Result<(), String> {
    use crate::repository::{CommandSpec, ProcessExecutor, SystemProcessExecutor};
    let (admin, common) = crate::project_location::checkout_identity(&relocation.new_root).ok_or("The selected checkout no longer has readable Git metadata")?;
    if common != relocation.destination { return Err("The selected checkout changed repositories while locating it".into()); }
    if admin != common { return Ok(()); }
    let command = || CommandSpec::new("git").arg("--git-dir").arg(&relocation.destination)
        .cwd(&relocation.new_root).env_remove("GIT_WORK_TREE").env_remove("GIT_COMMON_DIR")
        .timeout(Duration::from_secs(15)).output_limits(64 * 1024, 64 * 1024);
    let read = command().args(["config", "--local", "--null", "--get", "core.worktree"]);
    let output = SystemProcessExecutor.execute(&read, &CancellationToken::default()).map_err(|error| error.to_string())?;
    if output.exit_code == Some(1) { return Ok(()); }
    let output = output.require_success(&read.program).map_err(|error| error.to_string())?;
    let bytes = output.stdout.strip_suffix(&[0]).ok_or("Git returned an invalid checkout setting")?;
    let configured = PathBuf::from(String::from_utf8(bytes.to_vec()).map_err(|_| "Git checkout setting is not valid UTF-8")?);
    let absolute = if configured.is_absolute() { configured } else { relocation.destination.join(configured) };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            std::path::Component::CurDir => {},
            std::path::Component::ParentDir => { normalized.pop(); },
            component => normalized.push(component.as_os_str()),
        }
    }
    if normalized == relocation.new_root { return Ok(()); }
    if normalized != relocation.old_root {
        return Err(format!("This Git directory names another checkout: {}. Its setting was preserved.", normalized.display()));
    }
    let write = command().args(["config", "--local", "core.worktree"]).arg(&relocation.new_root);
    SystemProcessExecutor.execute(&write, &CancellationToken::default())
        .and_then(|output| output.require_success(&write.program)).map_err(|error| error.to_string())?;
    Ok(())
}

impl Cagents {
    pub(super) fn remember_project_binding(&mut self, id: ProjectId, shared: &crate::workspace_model::ModelProject) {
        if shared.ordinal == crate::workspace_model::PENDING_ORDINAL { return; }
        let root = shared.primary_checkout.clone().or_else(|| shared.checkout.clone()).or_else(|| (shared.kind == ProjectKind::Folder).then(|| shared.repository.clone()));
        let binding = self.durable.shared_project_bindings.get(&id);
        let next = match binding {
            None => Some(crate::model::SharedProjectBinding { ordinal: shared.ordinal, root }),
            Some(previous) if previous.ordinal == shared.ordinal && previous.root.is_none() && root.is_some() => Some(crate::model::SharedProjectBinding { ordinal: shared.ordinal, root }),
            _ => None,
        };
        if let Some(next) = next { Arc::make_mut(&mut self.durable).shared_project_bindings.insert(id, next); self.persist(); }
    }

    pub(super) fn project_root(&self, project: &SidebarProject) -> PathBuf {
        self.durable.shared_project_bindings.get(&project.id).and_then(|binding| binding.root.clone())
            .or_else(|| self.project_ui.primary_checkout_roots.get(&project.repository).cloned())
            .or_else(|| self.project_ui.checkout_roots.get(&project.repository).cloned())
            .unwrap_or_else(|| root(project))
    }
    pub(super) fn project_missing(&self, project: ProjectId) -> bool {
        self.sidebar_model.project(project).is_some_and(|project| project.missing)
    }
    fn project_cards(&self, id: ProjectId) -> HashSet<CardId> {
        let Some(project) = self.sidebar_model.project(id) else { return HashSet::new(); };
        let owned_roots = std::iter::once(self.project_root(project)).chain(std::iter::once(project.repository.clone())).chain(project.worktrees.iter().map(|worktree| worktree.path.clone())).collect::<Vec<_>>();
        let protected = self.sidebar_model.projects.iter().filter(|project| project.id != id).flat_map(|project| std::iter::once(self.project_root(project)).chain(std::iter::once(project.repository.clone())).chain(project.worktrees.iter().map(|worktree| worktree.path.clone()))).collect::<Vec<_>>();
        let owns_path = |path: &Path| {
            let depth = |roots: &[PathBuf]| roots.iter().filter(|root| path.starts_with(root)).map(|root| root.components().count()).max();
            match (depth(&owned_roots), depth(&protected)) {
                (Some(owned), Some(other)) => owned > other,
                (Some(_), None) => true,
                _ => false,
            }
        };
        let channel_owners = self.durable.channels.iter().flat_map(|channel| channel.cards.iter().map(move |card| (*card, channel.project_id))).collect::<HashMap<_,_>>();
        self.durable.cards.iter().filter(|card| {
            if let Some(project) = channel_owners.get(&card.id) { return *project == id; }
            match &card.kind {
                CardKind::Repository(view) => view.project == id,
                CardKind::PullRequests(view) => view.repository == project.repository,
                CardKind::Terminal(session) => self.durable.card_scopes.get(&card.id).map_or_else(|| owns_path(&session.launch.working_directory), |path| owns_path(path)),
                CardKind::Chat(session) => self.durable.card_scopes.get(&card.id).map_or_else(|| owns_path(&session.launch.recipe.working_directory), |path| owns_path(path)),
                _ => self.durable.card_scopes.get(&card.id).is_some_and(|path| owns_path(path)),
            }
        }).map(|card| card.id).collect()
    }
    pub(super) fn missing_project_cards(&self) -> HashSet<CardId> {
        self.sidebar_model.projects.iter().filter(|project| project.missing).flat_map(|project| self.project_cards(project.id)).collect()
    }
    pub(super) fn recovery_subscription(&self) -> Subscription<Message> {
        if crate::device::is_remote() || self.sidebar_model.projects.is_empty() { return Subscription::none(); }
        cosmic::iced::time::every(Duration::from_secs(2)).map(|_| Message::ProjectRecovery(Action::Tick))
    }
    pub(super) fn update_project_recovery(&mut self, action: Action) -> Task<Message> {
        match action {
            Action::Tick => {
                if self.project_recovery_ui.checking { return Task::none(); }
                let projects = self.sidebar_model.projects.iter().map(|project| (project.id, project.repository.clone(), self.project_root(project))).collect::<Vec<_>>();
                self.project_recovery_ui.checking = true;
                cosmic::task::future(async move {
                    let checked = tokio::task::spawn_blocking(move || projects.into_iter().map(|(id,path,root)| (id,path,project_recovery::missing_reason(&root))).collect()).await.unwrap_or_default();
                    Message::ProjectRecovery(Action::Checked(checked))
                })
            }
            Action::Checked(projects) => {
                self.project_recovery_ui.checking = false;
                let mut tasks = Vec::new();
                for (id, path, reason) in projects {
                    let missing = reason.is_some();
                    let known_root = self.sidebar_model.project(id).map(|project| self.project_root(project));
                    let Some(project) = self.sidebar_model.projects.iter_mut().find(|project| project.id == id && project.repository == path) else { continue; };
                    let detail = reason.map(|reason| format!("{}: {reason}", known_root.as_deref().unwrap_or(&path).display()));
                    if project.missing == missing {
                        if missing { project.unreachable = detail; }
                        continue;
                    }
                    project.missing = missing;
                    if missing {
                        project.unreachable = detail;
                        project.reloading = false;
                        self.refresh_ui.git.forget(id);
                        self.repository_generations.remove(&id);
                        if let Some(cancel) = self.refresh_ui.projects.remove(&id) { cancel.cancel(); }
                        if let Some(cancel) = self.repository_pr_cancellations.remove(&id) { cancel.cancel(); }
                        if let Some(key) = self.sidebar_state.active_worktree().cloned().filter(|key| key.project_id == id) {
                            self.cancel_source_read();
                            self.sidebar_model.source_control = Some(SourceControlModel::failed(key, "This project folder was moved or deleted. Locate it from Projects."));
                        }
                    } else {
                        project.unreachable = None;
                        tasks.push(self.project_load_task(path, Some(id), false, None));
                    }
                }
                Task::batch(tasks)
            }
            Action::VerifyProject(id) => {
                if self.modal_is_open() || !self.project_recovery_ui.locating.insert(id) { return Task::none(); }
                let Some(project) = self.sidebar_model.project(id) else { self.finish_project_location(id); return Task::none(); };
                let root = self.project_root(project);
                let relocation = Relocation { repository: project.repository.clone(), destination: project.repository.clone(), old_root: root.clone(), new_root: root };
                self.update_project_recovery(Action::Prepared(id, Ok(relocation)))
            }
            Action::SessionDetails(card) => {
                if !self.project_recovery_ui.details.insert(card) { self.project_recovery_ui.details.remove(&card); }
                Task::none()
            }
            Action::Command(id, ProjectRecoveryAction::Details) => {
                if let Some(project) = self.sidebar_model.projects.iter_mut().find(|project| project.id == id) { project.missing_details = !project.missing_details; }
                Task::none()
            }
            Action::Command(id, ProjectRecoveryAction::Remove) => self.handle_project_action(project_ui::Action::Command { project_id: id, worktree_id: None, command: sidebar::ProjectCommand::Remove }),
            Action::Command(id, ProjectRecoveryAction::ClosePanes) => {
                let owned = self.project_cards(id);
                let cards = self.durable.cards.iter().filter(|card| card.existence.is_open() && owned.contains(&card.id)).map(|card| card.id).collect();
                self.close_cards_in_order(cards)
            }
            Action::Command(id, ProjectRecoveryAction::Locate) => {
                if !self.project_recovery_ui.locating.insert(id) { return Task::none(); }
                if let Some(project) = self.sidebar_model.projects.iter_mut().find(|project| project.id == id) { project.reloading = true; }
                cosmic::task::future(async move {
                    let result = file_chooser::open::Dialog::new().title("Locate project folder").open_folder().await.map_err(|error| error.to_string()).and_then(|response| response.url().to_file_path().map_err(|()| "Select a local folder".into()));
                    Message::ProjectRecovery(Action::Located(id, result))
                })
            }
            Action::Located(id, result) => {
                let path = match result { Ok(path) => path, Err(error) => {
                    self.finish_project_location(id);
                    if !error.to_lowercase().contains("cancel") { self.status = Some(TransientNotice::error(error)); }
                    return Task::none();
                }};
                let Some(project) = self.sidebar_model.project(id) else { self.finish_project_location(id); return Task::none(); };
                let old_root = self.project_root(project);
                let repository = project.repository.clone();
                let kind = project.kind;
                let worktrees = project.worktrees.iter().map(|worktree| worktree.path.clone()).collect::<Vec<_>>();
                let open_repositories = self.sidebar_model.projects.iter().filter(|project| project.id != id).map(|project| project.repository.clone()).collect::<Vec<_>>();
                cosmic::task::future(async move {
                    let result = tokio::task::spawn_blocking(move || {
                        let new_root = path.canonicalize().map_err(|error| error.to_string())?;
                        let destination = if kind == ProjectKind::Folder { new_root.clone() } else {
                            crate::project_location::checkout_identity(&new_root).map(|(_, common)| common).ok_or("Choose the root folder of a Git checkout for this project")?
                        };
                        if open_repositories.contains(&destination) { return Err("That folder is already open as another project".into()); }
                        let relocation = Relocation { repository, destination, old_root, new_root };
                        relocation.validate()?;
                        if kind == ProjectKind::Git {
                            repair_checkout_config(&relocation)?;
                            use crate::repository::{CommandSpec, ProcessExecutor, SystemProcessExecutor};
                            let mut repair = CommandSpec::new("git").cwd(&relocation.new_root).args(["worktree", "repair"]).timeout(Duration::from_secs(15)).output_limits(64 * 1024, 64 * 1024);
                            for path in worktrees {
                                let path = relocation.mapped(&path);
                                if path != relocation.new_root && belongs_to_relocated_worktree(&path, &relocation) { repair = repair.arg(path); }
                            }
                            if let Err(error) = SystemProcessExecutor.execute(&repair, &CancellationToken::default()).and_then(|output| output.require_success(&repair.program)) {
                                // One separately missing linked folder must not
                                // prevent locating the primary checkout. Git
                                // reports that folder on the subsequent load.
                                tracing::warn!(%error, "some linked worktrees could not be repaired after locating project");
                            }
                        }
                        Ok(relocation)
                    }).await.map_err(|error| error.to_string()).and_then(|result| result);
                    Message::ProjectRecovery(Action::Prepared(id, result))
                })
            }
            Action::Prepared(id, result) => {
                let relocation = match result { Ok(relocation) => relocation, Err(error) => { self.finish_project_location(id); self.status = Some(TransientNotice::error(error)); return Task::none(); } };
                if !self.sidebar_model.projects.iter().any(|project| project.id == id && project.repository == relocation.repository) {
                    self.finish_project_location(id);
                    return Task::none();
                }
                if self.sidebar_model.projects.iter().any(|project| project.id != id && project.repository == relocation.destination) {
                    self.finish_project_location(id);
                    self.status = Some(TransientNotice::warning("That folder is already open as another project")); return Task::none();
                }
                if self.cards.iter().any(|(card,runtime)| matches!(runtime, CardRuntime::Repository(runtime) if runtime.is_saving()) && self.durable.card_scopes.get(card).is_some_and(|path| path.starts_with(&relocation.old_root))) {
                    self.finish_project_location(id);
                    self.status = Some(TransientNotice::warning("Wait for the current save to finish, then locate the project again."));
                    return Task::none();
                }
                let edit = crate::workspace_model::ModelEdit::RelocateProject { relocation: relocation.clone() };
                cosmic::task::future(async move {
                    let result = tokio::task::spawn_blocking(move || {
                        let socket = crate::terminal::ensure_daemon_ready().map_err(|error| error.to_string())?;
                        crate::control::ControlClient::connect(&socket).map_err(|error| error.to_string())?.edit_workspace_model(edit).map_err(|error| error.to_string())
                    }).await.map_err(|error| error.to_string()).and_then(|result| result);
                    Message::ProjectRecovery(Action::Applied(id, relocation, result))
                })
            }
            Action::Applied(id, relocation, result) => {
                self.finish_project_location(id);
                match result {
                Ok(model) => {
                    if model.project(&relocation.destination).is_none() {
                        self.status = Some(TransientNotice::warning("The project location changed while locating it. Refresh and try again."));
                        return Task::none();
                    }
                    if model.identity == self.workspace_model.identity && model.sequence < self.workspace_model.sequence {
                        return self.apply_project_relocation(&relocation);
                    }
                    self.workspace_model = model;
                    let relocated = if relocation.verification_only() { Task::none() } else { self.apply_project_relocation(&relocation) };
                    let refused = self.project_cards(id).into_iter().filter(|card| self.card_location_refusal(*card).is_some()).count();
                    self.status = Some(if refused > 0 {
                        TransientNotice::warning(format!("Project located. {refused} panes still need directory verification; use Verify again or Close pane in each warning."))
                    } else if relocation.verification_only() {
                        TransientNotice::success("Session locations verified.")
                    } else {
                        TransientNotice::success("Project located. Existing schedules are paused until their target is updated.")
                    });
                    relocated
                }
                Err(error) => { self.status = Some(TransientNotice::error(format!("Could not locate project: {error}"))); Task::none() }
                }
            }
        }
    }

    fn finish_project_location(&mut self, id: ProjectId) {
        self.project_recovery_ui.locating.remove(&id);
        if let Some(project) = self.sidebar_model.projects.iter_mut().find(|project| project.id == id) { project.reloading = false; }
    }

    pub(super) fn card_location_refusal(&self, card: CardId) -> Option<&crate::workspace_model::SessionLocationRefusal> {
        let state = self.card_state(card)?;
        if !state.existence.is_open() { return None; }
        let session = match &state.kind {
            CardKind::Chat(session) => session.id,
            CardKind::Terminal(session) => session.id,
            _ => return None,
        };
        self.workspace_model.session_location_refusals.iter().find(|refusal| refusal.session == session)
    }

    pub(super) fn project_location_notice(&self, card: CardId) -> Option<Element<'_, Message>> {
        let refusal = self.card_location_refusal(card)?;
        let expanded = self.project_recovery_ui.details.contains(&card);
        let project = self.durable.channels.iter().find(|channel| channel.cards.contains(&card)).map(|channel| channel.project_id)
            .or_else(|| self.durable.card_scopes.get(&card).and_then(|path| self.sidebar_model.worktree_at(path)).map(|key| key.project_id));
        let enabled = !self.modal_is_open();
        let mut actions = Vec::new();
        if let Some(project) = project {
            actions.push(crate::ui_controls::button::quiet("Verify again").id(card_control_id(card,"project.location.retry")).on_press_maybe((enabled && !self.project_recovery_ui.locating.contains(&project)).then_some(Message::ProjectRecovery(Action::VerifyProject(project)))).into());
        }
        actions.push(crate::ui_controls::button::quiet("Close pane").on_press_maybe(enabled.then_some(Message::CloseCard(card))).into());
        actions.push(crate::ui_controls::button::quiet(if expanded { "Hide details" } else { "Details" }).on_press_maybe(enabled.then_some(Message::ProjectRecovery(Action::SessionDetails(card)))).into());
        let mut body = widget::column::with_children(vec![
            notice_body("This session still needs a verified directory."),
            notice_surface_detail(NoticeSeverity::Warning, if expanded { format!("{}\n{}", refusal.directory.display(), refusal.reason) } else {
                let mut reason = refusal.reason.chars().take(160).collect::<String>();
                if refusal.reason.chars().count() > 160 { reason.push('…'); }
                reason
            }),
        ]).spacing(space::XS).width(Length::Fill);
        body = body.push(widget::row::with_children(actions).spacing(space::XS));
        Some(notice_surface(NoticeSeverity::Warning, body.into(), None, false))
    }

    /// A reconnect can miss Locate's watch edge. Recover logical ownership
    /// from the non-reused registry project rank or exact channel/agent IDs before
    /// honoring the old project's tombstone. Paths alone prove no association.
    pub(super) fn recover_shared_project_relocations(&mut self) -> Task<Message> {
        if self.workspace_model.identity == crate::workspace_model::NO_REGISTRY || self.durable.workspace_model_identity != Some(self.workspace_model.identity) { return Task::none(); }
        let mut relocations = Vec::new();
        for project in &self.sidebar_model.projects {
            let mut repositories = HashSet::new();
            if let Some(binding) = self.durable.shared_project_bindings.get(&project.id) {
                repositories.extend(self.workspace_model.projects.iter().filter(|shared| shared.ordinal == binding.ordinal).map(|shared| shared.repository.clone()));
            } else {
                // Channels cannot change project association except in an
                // explicit project relocation. A pane ID alone is insufficient:
                // panes can legitimately be transferred between projects.
                repositories.extend(self.durable.channels.iter().filter(|channel| channel.project_id == project.id && channel.shared).filter_map(|channel| self.workspace_model.channel(channel.id)).map(|channel| channel.repository.clone()));
                let owned = self.project_cards(project.id);
                for location in &self.workspace_model.agent_locations {
                    if owned.contains(&CardId::from_uuid(location.agent.as_uuid())) { repositories.insert(location.repository.clone()); }
                }
            }
            if repositories.len() != 1 { continue; }
            let destination = repositories.into_iter().next().unwrap();
            let Some(located) = self.workspace_model.project(&destination) else { continue; };
            let old_root = self.project_root(project);
            // An external metadata key can stay unchanged when its primary
            // moves. Ordinary opens of a linked checkout must not move it.
            if destination == project.repository && located.primary_checkout.as_ref().is_none_or(|root| root == &old_root) { continue; }
            let new_root = located.primary_checkout.clone().or_else(|| located.checkout.clone()).unwrap_or_else(|| if located.kind == ProjectKind::Git && destination.file_name().is_some_and(|name| name == ".git") { destination.parent().unwrap_or(&destination).to_path_buf() } else { destination.clone() });
            let relocation = Relocation { repository: project.repository.clone(), destination, old_root, new_root };
            if relocation.validate().is_ok() { relocations.push(relocation); }
        }
        Task::batch(relocations.into_iter().map(|relocation| self.apply_project_relocation(&relocation)))
    }

    pub(super) fn apply_project_relocation(&mut self, relocation: &Relocation) -> Task<Message> {
        if relocation.verification_only() { return Task::none(); }
        let Some(id) = self.sidebar_model.projects.iter().find(|project| project.repository == relocation.repository).map(|project| project.id) else { return Task::none(); };
        let owned = self.project_cards(id);
        for roots in [&mut self.project_ui.checkout_roots, &mut self.project_ui.primary_checkout_roots] {
            if let Some(previous) = roots.remove(&relocation.repository) { roots.insert(relocation.destination.clone(), relocation.mapped(&previous)); }
        }
        if let Some(binding) = Arc::make_mut(&mut self.durable).shared_project_bindings.get_mut(&id) { binding.root = Some(binding.root.as_deref().map_or_else(|| relocation.new_root.clone(), |root| relocation.mapped(root))); }
        let project = self.sidebar_model.projects.iter_mut().find(|project| project.id == id).unwrap();
        project.relocate(relocation);
        if let Some(session) = &self.session {
            session.save_project(ProjectState { id, generation: 0, name: project.name.clone(), repository: project.repository.clone(), kind: project.kind, existence: Existence::Open });
            session.save_worktrees(project.worktrees.iter().map(|worktree| WorktreeState { id: worktree.id, project_id: id, generation: 0, path: worktree.path.clone(), branch: worktree.branch.clone(), existence: Existence::Open }));
        }
        self.refresh_ui.git.forget(id);
        let durable = Arc::make_mut(&mut self.durable);
        for card in &mut durable.cards { if owned.contains(&card.id) { relocation.card(&mut card.kind); } }
        for (card,path) in &mut durable.card_scopes { if owned.contains(card) { relocation.path(path); } }
        for (card, navigation) in &mut durable.repository_navigation { if owned.contains(card) { relocation.path(&mut navigation.worktree); } }
        if let Some(path) = &mut durable.active_worktree { if self.sidebar_state.active_worktree().is_some_and(|key| key.project_id == id) { relocation.path(path); } }
        durable.worktree_focus = std::mem::take(&mut durable.worktree_focus).into_iter().map(|(path,card)| (if owned.contains(&card) { relocation.mapped(&path) } else { path },card)).collect();
        if let Some(path) = &mut self.active_workspace { if self.sidebar_state.active_worktree().is_some_and(|key| key.project_id == id) { relocation.path(path); } }
        self.worktree_focus = std::mem::take(&mut self.worktree_focus).into_iter().map(|(path,card)| (if owned.contains(&card) { relocation.mapped(&path) } else { path },card)).collect();
        self.known_worktree_roots = std::mem::take(&mut self.known_worktree_roots).into_iter().map(|path| { if relocation.old_root == path || self.sidebar_model.project(id).is_some_and(|project| project.worktrees.iter().any(|worktree| relocation.mapped(&path) == worktree.path)) { relocation.mapped(&path) } else { path } }).collect();
        for (card,path) in &mut self.worktree_owners { if owned.contains(card) { relocation.path(path); } }
        let mut tasks = Vec::new();
        for (card,runtime) in &mut self.cards {
            if !owned.contains(card) { continue; }
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
        let editors = self.cards.iter().filter_map(|(card,runtime)| matches!(runtime, CardRuntime::Repository(_)).then_some(*card)).collect::<Vec<_>>();
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
        app.sidebar_model.projects.push(SidebarProject::new(id, RepositoryId::new(), "Fixture".into(), root.to_path_buf(), Vec::new(), Vec::new()).with_kind(ProjectKind::Folder));
        (app,id)
    }
    #[test]
    fn reconnect_follows_non_reused_project_identity_even_when_old_path_is_reopened() {
        let old = Path::new("/old/project");
        let (mut app,id) = app(old);
        let identity = uuid::Uuid::new_v4();
        let durable = Arc::make_mut(&mut app.durable);
        durable.workspace_model_identity = Some(identity);
        durable.shared_project_bindings.insert(id, crate::model::SharedProjectBinding { ordinal: 7, root: Some(old.into()) });
        app.workspace_model = crate::workspace_model::WorkspaceModel { identity, projects: vec![
            crate::workspace_model::ModelProject { checkout: None, primary_checkout: None, repository: "/moved/project".into(), name: "Original".into(), kind: ProjectKind::Folder, ordinal: 7 },
            crate::workspace_model::ModelProject { checkout: None, primary_checkout: None, repository: old.into(), name: "Replacement".into(), kind: ProjectKind::Folder, ordinal: 8 },
        ], ..Default::default() };
        drop(app.recover_shared_project_relocations());
        assert_eq!(app.sidebar_model.project(id).unwrap().repository, Path::new("/moved/project"));
        assert_eq!(app.durable.shared_project_bindings[&id].ordinal, 7);
        // A fresh project at the reused path is not associated with rank7.
        let fresh = ProjectId::new();
        app.sidebar_model.projects.push(SidebarProject::new(fresh, RepositoryId::new(), "Replacement".into(), old.into(), Vec::new(), Vec::new()).with_kind(ProjectKind::Folder));
        Arc::make_mut(&mut app.durable).shared_project_bindings.insert(fresh, crate::model::SharedProjectBinding { ordinal: 8, root: Some(old.into()) });
        drop(app.recover_shared_project_relocations());
        assert_eq!(app.sidebar_model.project(fresh).unwrap().repository, old);
    }

    #[test]
    fn reconnect_does_not_interpret_a_replacement_registry_rank_as_the_old_project() {
        let old = Path::new("/old/project");
        let (mut app,id) = app(old);
        let durable = Arc::make_mut(&mut app.durable);
        durable.workspace_model_identity = Some(uuid::Uuid::new_v4());
        durable.shared_project_bindings.insert(id, crate::model::SharedProjectBinding { ordinal: 7, root: Some(old.into()) });
        app.workspace_model = crate::workspace_model::WorkspaceModel { identity: uuid::Uuid::new_v4(), projects: vec![crate::workspace_model::ModelProject { checkout: None, primary_checkout: None, repository: "/unrelated/project".into(), name: "Other".into(), kind: ProjectKind::Folder, ordinal: 7 }], ..Default::default() };
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
            let output = std::process::Command::new("git").current_dir(fixture.path()).args(args).output().unwrap();
            assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr)); output
        };
        git(&["init".as_ref(), "--separate-git-dir".as_ref(), metadata.as_os_str(), old.as_os_str()]);
        // Relative values are relative to the Git directory, including `..`.
        git(&["--git-dir".as_ref(), metadata.as_os_str(), "config".as_ref(), "core.worktree".as_ref(), "../old checkout".as_ref()]);
        let (mut app, id) = app(&metadata);
        app.sidebar_model.projects.iter_mut().find(|project| project.id == id).unwrap().kind = ProjectKind::Git;
        Arc::make_mut(&mut app.durable).shared_project_bindings.insert(id, crate::model::SharedProjectBinding { ordinal: 1, root: Some(old.clone()) });
        std::fs::rename(&old, &new).unwrap();
        assert_eq!(app.project_root(app.sidebar_model.project(id).unwrap()), old);
        assert!(project_recovery::root_missing(&old));
        let message = super::super::tests::task_app_message(app.update_project_recovery(Action::Located(id, Ok(new.clone()))));
        let Message::ProjectRecovery(Action::Prepared(_, result)) = message else { panic!() };
        let relocation = result.unwrap();
        assert_eq!(relocation.repository, metadata);
        assert_eq!(relocation.destination, metadata);
        assert!(!relocation.verification_only());
        let output = std::process::Command::new("git").current_dir(&new).args(["rev-parse", "--show-toplevel"]).output().unwrap();
        assert!(output.status.success());
        assert_eq!(String::from_utf8(output.stdout).unwrap().trim(), new.to_str().unwrap());
        git(&["--git-dir".as_ref(), metadata.as_os_str(), "config".as_ref(), "core.worktree".as_ref(), "/unrelated/checkout".as_ref()]);
        assert!(repair_checkout_config(&relocation).unwrap_err().contains("another checkout"));
        let output = git(&["--git-dir".as_ref(), metadata.as_os_str(), "config".as_ref(), "--get".as_ref(), "core.worktree".as_ref()]);
        assert_eq!(String::from_utf8(output.stdout).unwrap().trim(), "/unrelated/checkout");
    }

    #[test]
    fn locating_moved_checkout_repairs_linked_worktree_paths() {
        let parent = tempfile::tempdir().unwrap();
        let old = parent.path().join("original");
        std::fs::create_dir(&old).unwrap();
        let git = |path: &Path, args: &[&str]| {
            let output = std::process::Command::new("git").current_dir(path).args(args).output().unwrap();
            assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        };
        git(&old, &["init", "-b", "main"]);
        git(&old, &["-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", "commit", "--allow-empty", "-m", "fixture"]);
        let linked = old.join("linked");
        git(&old, &["worktree", "add", "-b", "linked", linked.to_str().unwrap()]);
        let (mut app,id) = app(&old.join(".git"));
        let project = app.sidebar_model.projects.iter_mut().find(|project| project.id == id).unwrap();
        project.kind = ProjectKind::Git;
        project.worktrees.push(SidebarWorktree::from_durable(crate::model::WorktreeId::new(), linked, Some("linked".into())));
        let located = parent.path().join("moved");
        std::fs::rename(&old, &located).unwrap();
        let relocation = Relocation { repository: old.join(".git"), destination: located.join(".git"), old_root: old.clone(), new_root: located.clone() };
        assert!(belongs_to_relocated_worktree(&located.join("linked"), &relocation));
        let task = app.update_project_recovery(Action::Located(id, Ok(located.clone())));
        let result = super::super::tests::task_app_message(task);
        let Message::ProjectRecovery(Action::Prepared(_, result)) = result else { panic!() };
        let relocation = result.unwrap();
        assert_eq!(relocation.destination, located.join(".git"));
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
        std::fs::write(linked.join(".git"), format!("gitdir: {}\n", old.join(".git/worktrees/linked").display())).unwrap();
        std::fs::write(admin.join("gitdir"), fixture.path().join("unrelated/.git").display().to_string()).unwrap();
        let relocation = Relocation { repository: old.join(".git"), destination: new.join(".git"), old_root: old, new_root: new };
        assert!(!belongs_to_relocated_worktree(&linked, &relocation));
    }

    #[test]
    fn disappearing_root_marks_row_missing_and_returning_root_recovers() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("project"); std::fs::create_dir(&root).unwrap();
        let (mut app,id) = app(&root);
        drop(app.update_project_recovery(Action::Checked(vec![(id,root.clone(),None)])));
        assert!(!app.project_missing(id));
        std::fs::remove_dir(&root).unwrap();
        drop(app.update_project_recovery(Action::Checked(vec![(id,root.clone(),project_recovery::missing_reason(&root))])));
        assert!(app.project_missing(id));
        assert!(app.sidebar_model.project(id).unwrap().unreachable.is_some());
        std::fs::create_dir(&root).unwrap();
        drop(app.update_project_recovery(Action::Checked(vec![(id,root.clone(),project_recovery::missing_reason(&root))])));
        assert!(!app.project_missing(id));
    }
    #[test]
    fn session_location_warning_stays_with_exact_pane_and_clears_after_verified_model() {
        let (mut app, _) = app(Path::new("/old/project"));
        drop(app.add_terminal("/old/project".into()));
        let card = app.durable.cards.last().unwrap().id;
        let CardKind::Terminal(session) = &app.card_state(card).unwrap().kind else { panic!() };
        let session = session.id;
        app.workspace_model.session_location_refusals.push(crate::workspace_model::SessionLocationRefusal { session, directory: "/new/project".into(), reason: "The owned provider has not confirmed this directory; retry when ready.".into() });
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
        let CardKind::Terminal(session) = &mut app.card_state_mut(card).unwrap().kind else { panic!() };
        session.continuity = TerminalContinuity::Live { host_session_id: HostSessionId::new("fixture"), boot_id: crate::model::BootId::new("fixture"), host_incarnation: HostIncarnation::new("fixture") };
        let original = app.card_state(card).unwrap().kind.clone();
        Arc::make_mut(&mut app.durable).card_scopes.insert(card, old.into());
        app.terminal_working_directories.insert(card, old.into());
        let relocation = Relocation { repository: old.into(), destination: "/new/project".into(), old_root: old.into(), new_root: "/new/project".into() };
        drop(app.apply_project_relocation(&relocation));
        assert_eq!(app.card_state(card).unwrap().kind, original, "logical placement must not rewrite immutable process launch provenance");
        assert_eq!(app.durable.card_scopes[&card], relocation.new_root);
        assert_eq!(app.terminal_working_directories[&card], old);
    }

    #[test]
    fn missing_parent_does_not_claim_independently_open_nested_project() {
        let old = Path::new("/old/project");
        let (mut app, id) = app(old);
        let nested = ProjectId::new();
        app.sidebar_model.projects.push(SidebarProject::new(nested, RepositoryId::new(), "Nested".into(), old.join("nested"), Vec::new(), Vec::new()).with_kind(ProjectKind::Folder));
        let card = CardId::new();
        Arc::make_mut(&mut app.durable).cards.push(CardState { id: card, generation: 0, title: "Nested".into(), width_fraction: 0.5, existence: Existence::Open, exposure: Exposure::Offscreen, residency: Residency::Cold, kind: CardKind::Repository(RepositoryView { project: nested, repository: old.join("nested"), worktree: old.join("nested"), selected_file: None }) });
        Arc::make_mut(&mut app.durable).card_scopes.insert(card, old.join("nested"));
        app.sidebar_model.projects.iter_mut().find(|project| project.id == id).unwrap().missing = true;
        assert!(!app.missing_project_cards().contains(&card));
        let relocation = Relocation { repository: old.into(), destination: "/new/project".into(), old_root: old.into(), new_root: "/new/project".into() };
        drop(app.apply_project_relocation(&relocation));
        assert_eq!(app.durable.card_scopes[&card], old.join("nested"));
        let CardKind::Repository(view) = &app.card_state(card).unwrap().kind else { panic!() };
        assert_eq!(view.worktree, old.join("nested"));
    }

    #[test]
    fn locate_repoints_durable_project_and_cards_then_schedules_git_refresh() {
        let old = Path::new("/old/project");
        let (mut app,id) = app(old);
        let view = RepositoryView { project: id, repository: old.into(), worktree: old.join("nested"), selected_file: Some("file.rs".into()) };
        let card = CardId::new();
        Arc::make_mut(&mut app.durable).cards.push(CardState { id: card, generation: 0, title: "File".into(), width_fraction: 0.5, existence: Existence::Open, exposure: Exposure::Offscreen, residency: Residency::Cold, kind: CardKind::Repository(view) });
        let relocation = Relocation { repository: old.into(), destination: "/new/project".into(), old_root: old.into(), new_root: "/new/project".into() };
        drop(app.apply_project_relocation(&relocation));
        assert_eq!(app.sidebar_model.project(id).unwrap().repository, relocation.destination);
        let CardKind::Repository(view) = &app.card_state(card).unwrap().kind else { panic!() };
        assert_eq!(view.worktree, Path::new("/new/project/nested"));
        assert!(app.refresh_ui.projects.contains_key(&id), "locate schedules a fresh project/Git load");
    }
}
