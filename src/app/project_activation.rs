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
        self.project_ui.activation = Some(PendingActivation { repository, loading: None });
    }

    pub(super) fn finish_project_activation_load(&mut self, project: ProjectId, succeeded: bool) {
        self.project_ui.pending_loads.retain(|_, owner| *owner != project);
        if let Some(pending) = self.project_ui.activation.as_mut()
            && pending.loading == Some(project)
        {
            if succeeded { pending.loading = None; }
            else { self.project_ui.activation = None; }
        }
    }

    pub(super) fn resume_project_activation(&mut self) -> Task<Message> {
        if self.update_prepared || self.modal_is_open() || self.focused_repository_is_saving() {
            return Task::none();
        }
        let Some(pending) = self.project_ui.activation.as_ref() else { return Task::none(); };
        if pending.loading.is_some() { return Task::none(); }
        let repository = pending.repository.clone();
        if self.project_ui.closing.values().any(|path| path == &repository) {
            self.project_ui.activation = None;
            self.status = Some(TransientNotice::warning("Wait for this project to finish closing before reopening it."));
            return Task::none();
        }
        if let Some(project) = self.sidebar_model.projects.iter()
            .find(|project| project.repository == repository && project.unreachable.is_none())
        {
            let path = if project.kind == ProjectKind::Folder { Some(project.repository.clone()) }
                else { project.worktrees.iter()
                    .find(|row| row.path != repository && sidebar::is_primary_worktree(&repository, &row.path))
                    .or_else(|| project.worktrees.iter().find(|row| row.path != repository))
                    .map(|row| row.path.clone()) };
            self.project_ui.activation = None;
            return if let Some(path) = path {
                if self.refuse_in_deleting_worktree(&path) { return Task::none(); }
                Task::batch([self.select_worktree(path, false), self.activate_main_window(None)])
            } else {
                self.status = Some(TransientNotice::warning("This project has no available checkout to select."));
                Task::none()
            };
        }
        if let Some(id) = self.project_ui.pending_loads.get(&repository).copied()
            .filter(|id| self.repository_generations.contains_key(id))
        {
            self.project_ui.activation.as_mut().unwrap().loading = Some(id);
            return Task::none();
        }
        let project = self.sidebar_model.projects.iter().find(|project| project.repository == repository);
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
    matches!(message,
        Message::OpenRepository | Message::FocusCard(_) | Message::FocusAdjacent(_)
        | Message::ActivateSession { .. } | Message::NewTerminal | Message::NewBrowser
        | Message::OpenBrowser(_) | Message::OpenLink(..) | Message::OpenDocumentLink(..)
        | Message::ChromeUi(chrome_ui::Action::Launch(_))
        | Message::Viewport { event: WorkspaceViewportEvent::PointerFocused(_), .. }
        | Message::Sidebar(SidebarAction::OpenRepository { .. } | SidebarAction::OpenWorktree { .. }
            | SidebarAction::OpenChannel { .. } | SidebarAction::OpenPullRequest { .. }
            | SidebarAction::BrowsePullRequests { .. } | SidebarAction::NewChannel { .. }
            | SidebarAction::NewWorktree { .. } | SidebarAction::OpenSettings | SidebarAction::OpenAutomations)
    ) || matches!(message, Message::ChannelUi(action) if !action.answers_own_dialog())
}
