use super::*;

pub(super) struct State {
    pub(super) focused: bool,
    pub(super) source_generation: u64,
    pub(super) source_cancel: Option<CancellationToken>,
    pub(super) projects: HashMap<ProjectId, CancellationToken>,
    last_projects: Instant,
    last_pull_request: Instant,
}

impl Default for State {
    fn default() -> Self {
        Self {
            focused: false,
            source_generation: 0,
            source_cancel: None,
            projects: HashMap::new(),
            last_projects: Instant::now(),
            last_pull_request: Instant::now(),
        }
    }
}

impl Drop for State {
    fn drop(&mut self) {
        if let Some(cancel) = &self.source_cancel {
            cancel.cancel();
        }
        for cancel in self.projects.values() {
            cancel.cancel();
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) enum Action {
    Tick,
    Focused(window::Id, bool),
}

impl Cagents {
    pub(super) fn refresh_subscription(&self) -> Subscription<Message> {
        if !self.refresh_ui.focused || self.modal_is_open() || self.update_prepared {
            return Subscription::none();
        }
        cosmic::iced::time::every(Duration::from_secs(10)).map(|_| Message::RefreshUi(Action::Tick))
    }

    pub(super) fn update_refresh(&mut self, action: Action) -> Task<Message> {
        let regained_focus = match action {
            Action::Focused(window, focused) => {
                if self.core.main_window_id() != Some(window) {
                    return Task::none();
                }
                let regained = focused && !self.refresh_ui.focused;
                self.refresh_ui.focused = focused;
                if !regained {
                    return Task::none();
                }
                true
            }
            Action::Tick => false,
        };
        if !self.refresh_ui.focused || self.modal_is_open() || self.update_prepared {
            return Task::none();
        }
        let mut tasks = Vec::new();
        tasks.push(self.commit_terminal_handoffs());
        for (&card, runtime) in &mut self.cards {
            if self.workspace.visibility(card) == Some(VisibilityClass::Visible)
                && let CardRuntime::Repository(runtime) = runtime
            {
                tasks.push(repository_task(runtime.refresh_visible(card)));
            }
        }
        if self.context_inspector_open
            && matches!(self.context_tab, ContextTab::Files | ContextTab::Changes)
            && self.refresh_ui.source_cancel.is_none()
            && let Some(key) = self
                .sidebar_model
                .source_control
                .as_ref()
                .filter(|source| !source.busy())
                .map(|source| source.key.clone())
        {
            tasks.push(self.reload_source_control(&key));
        }
        let now = Instant::now();
        if regained_focus
            || now.duration_since(self.refresh_ui.last_pull_request) >= Duration::from_secs(30)
        {
            self.refresh_ui.last_pull_request = now;
            tasks.push(self.maybe_refresh_pull_request());
        }
        if regained_focus
            || now.duration_since(self.refresh_ui.last_projects) >= Duration::from_secs(60)
        {
            self.refresh_ui.last_projects = now;
            if let Some((id, path)) =
                sidebar::selected_project(&self.sidebar_model, &self.sidebar_state)
                    .map(|project| (project.id, project.repository.clone()))
                    .filter(|(id, _)| !self.refresh_ui.projects.contains_key(id))
            {
                tasks.push(self.repository_load_task(path, Some(id), false));
            }
        }
        Task::batch(tasks)
    }

    pub(super) fn cancel_source_read(&mut self) {
        if let Some(cancel) = self.refresh_ui.source_cancel.take() {
            cancel.cancel();
        }
        self.refresh_ui.source_generation = self.refresh_ui.source_generation.wrapping_add(1);
    }

    pub(super) fn repository_read_task(
        &mut self,
        path: PathBuf,
        project_id: ProjectId,
        generation: u64,
        open_cards: bool,
    ) -> Task<Message> {
        let cancellation = CancellationToken::default();
        if let Some(previous) = self
            .refresh_ui
            .projects
            .insert(project_id, cancellation.clone())
        {
            previous.cancel();
        }
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || {
                load_repository(&path, Some(project_id), cancellation)
            })
            .await
            .map_err(|error| format!("repository loader join failed: {error}"))
            .and_then(|result| result)
            .map(Box::new);
            Message::RepositoryLoaded {
                open_cards,
                project_id,
                generation,
                result,
            }
        })
    }
}
