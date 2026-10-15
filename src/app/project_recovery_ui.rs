//! Root availability is independent of Git. Missing projects use a cheap
//! filesystem probe until restored; no Git/GitHub polling is scheduled for them.
use super::*;
use crate::{project_recovery::{self, Relocation}, sidebar::ProjectRecoveryAction};

#[derive(Default)]
pub(super) struct State { checking: bool, locating: HashSet<ProjectId>, pub(super) closing_panes: bool }
#[derive(Debug, Clone)]
pub enum Action {
    Tick,
    Checked(Vec<(ProjectId, PathBuf, Option<String>)>),
    Command(ProjectId, ProjectRecoveryAction),
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

impl Cagents {
    pub(super) fn project_missing(&self, project: ProjectId) -> bool {
        self.sidebar_model.project(project).is_some_and(|project| project.missing)
    }
    fn project_cards(&self, id: ProjectId) -> HashSet<CardId> {
        let Some(project) = self.sidebar_model.project(id) else { return HashSet::new(); };
        let owned_roots = std::iter::once(root(project)).chain(project.worktrees.iter().map(|worktree| worktree.path.clone())).collect::<Vec<_>>();
        let protected = self.sidebar_model.projects.iter().filter(|project| project.id != id).flat_map(|project| std::iter::once(root(project)).chain(project.worktrees.iter().map(|worktree| worktree.path.clone()))).collect::<Vec<_>>();
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
                let projects = self.sidebar_model.projects.iter().map(|project| (project.id, project.repository.clone(), root(project))).collect::<Vec<_>>();
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
                    let Some(project) = self.sidebar_model.projects.iter_mut().find(|project| project.id == id && project.repository == path) else { continue; };
                    let detail = reason.map(|reason| format!("{}: {reason}", root(project).display()));
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
                let old_root = root(project);
                let repository = project.repository.clone();
                let kind = project.kind;
                let worktrees = project.worktrees.iter().map(|worktree| worktree.path.clone()).collect::<Vec<_>>();
                let open_repositories = self.sidebar_model.projects.iter().filter(|project| project.id != id).map(|project| project.repository.clone()).collect::<Vec<_>>();
                cosmic::task::future(async move {
                    let result = tokio::task::spawn_blocking(move || {
                        let new_root = path.canonicalize().map_err(|error| error.to_string())?;
                        let destination = if kind == ProjectKind::Folder { new_root.clone() } else {
                            crate::project_location::discover(&new_root).and_then(|location| if location.kind == ProjectKind::Git { Ok(location.key) } else { Err("Choose a Git checkout for this project".into()) })?
                        };
                        if open_repositories.contains(&destination) { return Err("That folder is already open as another project".into()); }
                        let relocation = Relocation { repository, destination, old_root, new_root };
                        relocation.validate()?;
                        if kind == ProjectKind::Git {
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
                    let relocated = self.apply_project_relocation(&relocation);
                    self.status = Some(TransientNotice::success("Project located. Existing schedules are paused until their target is updated."));
                    let _ = id;
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

    pub(super) fn apply_project_relocation(&mut self, relocation: &Relocation) -> Task<Message> {
        let Some(id) = self.sidebar_model.projects.iter().find(|project| project.repository == relocation.repository).map(|project| project.id) else { return Task::none(); };
        let owned = self.project_cards(id);
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
