//! One latest explicit CLI selection; ordinary registry adoption stays quiet.
use super::*;

#[derive(Debug)]
pub(super) struct PendingActivation {
    repository: PathBuf,
    loading: Option<ProjectId>,
}

impl Cagents {
    pub(super) fn request_project_activation(&mut self, repository: PathBuf) {
        if crate::device::is_remote() || !repository.is_absolute() {
            return;
        }
        self.cancel_workspace_preview();
        self.chrome_ui.close();
        self.project_ui.activation = Some(PendingActivation {
            repository,
            loading: None,
        });
    }

    pub(super) fn finish_project_activation_load(&mut self, project: ProjectId, succeeded: bool) {
        self.project_ui
            .pending_loads
            .retain(|_, owner| *owner != project);
        if let Some(pending) = self.project_ui.activation.as_mut()
            && pending.loading == Some(project)
        {
            if succeeded {
                pending.loading = None;
            } else {
                self.project_ui.activation = None;
            }
        }
    }

    pub(super) fn resume_project_activation(&mut self) -> Task<Message> {
        if crate::device::is_remote() {
            self.project_ui.activation = None;
            return Task::none();
        }
        if self.update_prepared || self.modal_is_open() || self.focused_repository_is_saving() {
            return Task::none();
        }
        let Some(pending) = self.project_ui.activation.as_ref() else {
            return Task::none();
        };
        if pending.loading.is_some() {
            return Task::none();
        }
        let repository = pending.repository.clone();
        if let Some(task) = self.defer_project_activation_for_work(repository.clone()) {
            return task;
        }
        if self
            .project_ui
            .closing
            .values()
            .any(|path| path == &repository)
        {
            self.project_ui.activation = None;
            self.status = Some(TransientNotice::warning(
                "Wait for this project to finish closing before reopening it.",
            ));
            return Task::none();
        }
        self.project_ui.closed.remove(&repository);
        if let Some(project) = self
            .sidebar_model
            .projects
            .iter()
            .find(|project| project.repository == repository && project.unreachable.is_none())
        {
            let path = if project.kind == ProjectKind::Folder {
                Some(project.repository.clone())
            } else {
                project
                    .worktrees
                    .iter()
                    .find(|row| {
                        row.path != repository
                            && sidebar::is_primary_worktree(&repository, &row.path)
                    })
                    .or_else(|| project.worktrees.iter().find(|row| row.path != repository))
                    .map(|row| row.path.clone())
            };
            if path.is_some() || !project.reloading {
                self.project_ui.activation = None;
                return if let Some(path) = path {
                    if self.refuse_in_deleting_worktree(&path) {
                        return Task::none();
                    }
                    Task::batch([
                        self.select_worktree(path, false),
                        self.activate_main_window(None),
                    ])
                } else {
                    self.status = Some(TransientNotice::warning(
                        "This project has no available checkout to select.",
                    ));
                    Task::none()
                };
            }
        }
        if let Some(id) = self
            .project_ui
            .pending_loads
            .get(&repository)
            .copied()
            .filter(|id| self.repository_generations.contains_key(id))
        {
            self.project_ui.activation.as_mut().unwrap().loading = Some(id);
            return Task::none();
        }
        let project = self
            .sidebar_model
            .projects
            .iter()
            .find(|project| project.repository == repository);
        let id = project.map_or_else(ProjectId::new, |project| project.id);
        let kind = project.map(|project| project.kind);
        self.project_ui.activation.as_mut().unwrap().loading = Some(id);
        // This is an explicit reopen. Unlike ordinary adoption, it may clear
        // this window's own close marker after the CLI published ReopenProject.
        self.project_ui.closed.remove(&repository);
        self.project_load_task(repository, Some(id), false, kind)
    }
}

/// Only admitted user navigation cancels a delayed CLI selection. Native
/// pointer-rest observations and background read completions are not choices.
pub(super) fn supersedes_project_activation(message: &Message) -> bool {
    matches!(
        message,
        Message::OpenRepository
            | Message::NewWindow
            | Message::WindowCloseRequested(_)
            | Message::FocusCard(_)
            | Message::FocusAdjacent(_)
            | Message::ActivateSession { .. }
            | Message::NewTerminal
            | Message::NewBrowser
            | Message::OpenBrowser(_)
            | Message::OpenLink(..)
            | Message::OpenDocumentLink(..)
            | Message::ChromeUi(chrome_ui::Action::Launch(_))
            | Message::RunCommand(_)
            | Message::Palette(palette_ui::Action::Run(_) | palette_ui::Action::Submit)
            | Message::ProjectUi(
                project_ui::Action::Command { .. }
                    | project_ui::Action::ConfirmDelete
                    | project_ui::Action::ConfirmRemove
                    | project_ui::Action::RetryDelete(_)
            )
            | Message::Viewport {
                event: WorkspaceViewportEvent::PointerFocused(_),
                ..
            }
            | Message::Sidebar(
                SidebarAction::ProjectCommand { .. }
                    | SidebarAction::OpenRepository { .. }
                    | SidebarAction::OpenWorktree { .. }
                    | SidebarAction::OpenChannel { .. }
                    | SidebarAction::OpenPullRequest { .. }
                    | SidebarAction::BrowsePullRequests { .. }
                    | SidebarAction::NewChannel { .. }
                    | SidebarAction::NewWorktree { .. }
                    | SidebarAction::OpenSettings
                    | SidebarAction::OpenAutomations
            )
    ) || matches!(message, Message::ChannelUi(action) if !action.answers_own_dialog())
        || matches!(message, Message::Repository(action) if !repository_action_allowed_while_modal(action))
        || matches!(message, Message::ChatUi(action) if !action.allowed_while_modal())
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    fn complete(app: &mut Cagents, task: Task<Message>) {
        let message = super::super::tests::task_app_message(task);
        assert!(matches!(message, Message::RepositoryLoaded { .. }));
        drop(app.update(message));
    }

    #[test]
    fn cli_selection_reuses_adoption_load_and_never_creates_a_terminal() {
        const TEST: &str = "app::project_activation::tests::cli_selection_reuses_adoption_load_and_never_creates_a_terminal";
        if !crate::test_git::isolated_test(TEST) {
            return;
        }
        for adoption_first in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path().canonicalize().unwrap();
            let mut app = super::super::tests::context_inspector_test_app(false);
            let before = app.durable.cards.len();
            if adoption_first {
                let task =
                    app.project_load_task(root.clone(), None, false, Some(ProjectKind::Folder));
                let sequence = app.repository_load_sequence;
                app.request_project_activation(root.clone());
                drop(app.resume_project_activation());
                assert_eq!(
                    app.repository_load_sequence, sequence,
                    "adoption owns the pending read"
                );
                complete(&mut app, task);
            } else {
                app.request_project_activation(root.clone());
                let first = app.resume_project_activation();
                let owner = app.project_ui.activation.as_ref().unwrap().loading.unwrap();
                let adopted =
                    app.project_load_task(root.clone(), None, false, Some(ProjectKind::Folder));
                assert_eq!(app.project_ui.pending_loads[&root], owner);
                complete(&mut app, first);
                complete(&mut app, adopted);
            }
            assert_eq!(app.active_workspace.as_ref(), Some(&root));
            assert_eq!(app.durable.cards.len(), before);
            assert!(app.project_ui.activation.is_none());
            assert_eq!(
                app.sidebar_model
                    .projects
                    .iter()
                    .filter(|project| project.repository == root)
                    .count(),
                1
            );
        }
    }

    #[test]
    fn late_load_cannot_replace_newer_cli_or_manual_selection() {
        const TEST: &str = "app::project_activation::tests::late_load_cannot_replace_newer_cli_or_manual_selection";
        if !crate::test_git::isolated_test(TEST) {
            return;
        }
        for manual in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let a = directory.path().join("a");
            let b = directory.path().join("b");
            std::fs::create_dir(&a).unwrap();
            std::fs::create_dir(&b).unwrap();
            let mut app = super::super::tests::context_inspector_test_app(false);
            if manual {
                let read = app.project_load_task(b.clone(), None, false, Some(ProjectKind::Folder));
                complete(&mut app, read);
            }
            app.request_project_activation(a.clone());
            let old = app.resume_project_activation();
            if manual {
                let id = app
                    .sidebar_model
                    .projects
                    .iter()
                    .find(|project| project.repository == b)
                    .unwrap()
                    .id;
                drop(app.update(Message::Sidebar(SidebarAction::OpenRepository {
                    project_id: id,
                    repository: b.clone(),
                })));
            } else {
                app.request_project_activation(b.clone());
                let newer = app.resume_project_activation();
                complete(&mut app, newer);
            }
            assert_eq!(app.active_workspace.as_ref(), Some(&b));
            complete(&mut app, old);
            assert_eq!(
                app.active_workspace.as_ref(),
                Some(&b),
                "older metadata may publish, but cannot navigate"
            );
            assert!(app.project_ui.activation.is_none());
        }
    }

    #[test]
    fn failed_cli_load_releases_its_intent_and_allows_repaired_retry() {
        const TEST: &str = "app::project_activation::tests::failed_cli_load_releases_its_intent_and_allows_repaired_retry";
        if !crate::test_git::isolated_test(TEST) {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("missing");
        std::fs::create_dir(&root).unwrap();
        let mut app = super::super::tests::context_inspector_test_app(false);
        let previous = app.active_workspace.clone();
        app.request_project_activation(root.clone());
        let read = app.resume_project_activation();
        std::fs::remove_dir(&root).unwrap();
        let failed = super::super::tests::task_app_message(read);
        assert!(matches!(
            &failed,
            Message::RepositoryLoaded { result: Err(_), .. }
        ));
        drop(app.update(failed));
        assert_eq!(app.active_workspace, previous);
        assert!(app.project_ui.activation.is_none());
        assert!(!app.project_ui.pending_loads.contains_key(&root));
        std::fs::create_dir(&root).unwrap();
        app.request_project_activation(root.clone());
        let read = app.resume_project_activation();
        complete(&mut app, read);
        assert_eq!(app.active_workspace.as_ref(), Some(&root));
    }

    #[test]
    fn cli_selection_never_navigates_to_a_bare_metadata_directory() {
        const TEST: &str = "app::project_activation::tests::cli_selection_never_navigates_to_a_bare_metadata_directory";
        if !crate::test_git::isolated_test(TEST) {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let bare = directory.path().join("metadata.git");
        crate::test_git::run(crate::test_git::isolated_command(
            crate::repository::CommandSpec::new("git")
                .args(["-c", "init.templateDir=", "init", "--bare", "--template="])
                .arg(bare.as_os_str()),
        ));
        let mut app = super::super::tests::context_inspector_test_app(false);
        let read = app.project_load_task(bare.clone(), None, false, Some(ProjectKind::Git));
        complete(&mut app, read);
        let previous = app.active_workspace.clone();
        app.request_project_activation(bare.clone());
        let sequence = app.repository_load_sequence;
        drop(app.resume_project_activation());
        assert_eq!(app.active_workspace, previous);
        assert!(app.project_ui.activation.is_none());
        assert_eq!(
            app.repository_load_sequence, sequence,
            "bare metadata must not trigger an endless reload"
        );
    }

    #[test]
    fn frame_resuming_a_ready_selection_rebinds_the_visible_scope_immediately() {
        const TEST: &str = "app::project_activation::tests::frame_resuming_a_ready_selection_rebinds_the_visible_scope_immediately";
        if !crate::test_git::isolated_test(TEST) {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let mut app = super::super::tests::context_inspector_test_app(false);
        app.request_project_activation(root.clone());
        let task = app.resume_project_activation();
        app.update_prepared = true;
        let previous = app.active_workspace.clone();
        complete(&mut app, task);
        assert_eq!(app.active_workspace, previous);
        app.update_prepared = false;
        drop(app.update(Message::Frame(Instant::now())));
        assert_eq!(app.active_workspace.as_ref(), Some(&root));
        assert_eq!(
            app.sidebar_state
                .active_worktree()
                .map(|key| &key.canonical_path),
            Some(&root)
        );
        assert_eq!(
            app.sidebar_model
                .source_control
                .as_ref()
                .map(|model| &model.key.canonical_path),
            Some(&root)
        );
    }

    #[test]
    fn ready_cli_selection_waits_for_the_focused_runtime_save_completion() {
        const TEST: &str = "app::project_activation::tests::ready_cli_selection_waits_for_the_focused_runtime_save_completion";
        if !crate::test_git::isolated_test(TEST) {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let mut app = super::super::tests::context_inspector_test_app(true);
        let card = app.workspace.focused().unwrap();
        app.request_project_activation(root.clone());
        let load = app.resume_project_activation();
        // This fixture owns a real Runtime SaveTicket; no application-level
        // saving flag is injected. The completion models durable success,
        // while the ordinary Repository reducer owns releasing navigation.
        let (runtime, _) = RepositoryRuntime::post_persist_failure_fixture(card);
        let completion = runtime.durable_active_save_completion();
        app.cards
            .insert(card, CardRuntime::Repository(Box::new(runtime)));
        assert!(app.focused_repository_is_saving());
        let previous = app.active_workspace.clone();
        complete(&mut app, load);
        assert_eq!(app.active_workspace, previous);
        assert!(app.project_ui.activation.is_some());
        drop(app.update(Message::Frame(Instant::now())));
        assert_eq!(app.active_workspace, previous);
        drop(app.update(Message::Repository(completion)));
        assert!(!app.focused_repository_is_saving());
        assert_eq!(app.active_workspace.as_ref(), Some(&root));
        assert!(app.project_ui.activation.is_none());
    }

    #[test]
    fn cli_selection_waits_for_modal_and_update_admission() {
        const TEST: &str =
            "app::project_activation::tests::cli_selection_waits_for_modal_and_update_admission";
        if !crate::test_git::isolated_test(TEST) {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let mut app = super::super::tests::context_inspector_test_app(false);
        app.update_prepared = true;
        app.request_project_activation(root.clone());
        let sequence = app.repository_load_sequence;
        drop(app.resume_project_activation());
        assert_eq!(app.repository_load_sequence, sequence);
        app.update_prepared = false;
        let task = app.resume_project_activation();
        drop(app.update(Message::ResetSettings));
        assert!(app.modal_is_open());
        let previous = app.active_workspace.clone();
        complete(&mut app, task);
        assert_eq!(
            app.active_workspace, previous,
            "metadata completion does not cross a modal"
        );
        assert!(app.project_ui.activation.is_some());
        drop(app.update(Message::CancelResetSettings));
        assert_eq!(app.active_workspace.as_ref(), Some(&root));
        assert!(app.project_ui.activation.is_none());
    }
}
