use super::*;

/// How long the selected project's rows may go unread while the window is
/// focused.
const SELECTED_PROJECT_INTERVAL: Duration = Duration::from_secs(60);
/// Every other project's rows draw the same pull request and check state, so
/// they are read as well, less often and one at a time.
const BACKGROUND_PROJECT_INTERVAL: Duration = Duration::from_secs(120);
/// A project something has made stale is still read at most this often, so an
/// agent finishing turn after turn cannot keep its project's reads going.
const STALE_PROJECT_INTERVAL: Duration = Duration::from_secs(30);

pub(super) struct State {
    pub(super) focused: bool,
    pub(super) source_generation: u64,
    pub(super) source_cancel: Option<CancellationToken>,
    pub(super) projects: HashMap<ProjectId, CancellationToken>,
    schedule: ProjectSchedule,
    last_pull_request: Instant,
}

impl Default for State {
    fn default() -> Self {
        Self {
            focused: false,
            source_generation: 0,
            source_cancel: None,
            projects: HashMap::new(),
            schedule: ProjectSchedule::default(),
            last_pull_request: Instant::now(),
        }
    }
}

/// When each project was last read, and which ones something has made worth
/// reading before their interval is up.
#[derive(Debug, Default)]
struct ProjectSchedule {
    read: HashMap<ProjectId, Instant>,
    stale: HashSet<ProjectId>,
}

impl ProjectSchedule {
    fn started(&mut self, project: ProjectId, now: Instant) {
        self.read.insert(project, now);
        self.stale.remove(&project);
    }

    fn retain(&mut self, open: impl Fn(ProjectId) -> bool) {
        self.read.retain(|&project, _| open(project));
        self.stale.retain(|&project| open(project));
    }

    fn mark_stale(&mut self, project: ProjectId) {
        self.stale.insert(project);
    }

    fn due(&self, project: ProjectId, interval: Duration, now: Instant) -> bool {
        let interval = if self.stale.contains(&project) {
            interval.min(STALE_PROJECT_INTERVAL)
        } else {
            interval
        };
        self.read
            .get(&project)
            .is_none_or(|&read| now.duration_since(read) >= interval)
    }

    /// The one project a refresh tick reads, if any. The selected project
    /// keeps its own cadence; the rest wait until no read is in flight, so a
    /// long project list costs one `git` and `gh` round at a time rather than
    /// a burst, and the one read longest ago goes first.
    fn next(
        &self,
        selected: Option<ProjectId>,
        others: impl IntoIterator<Item = ProjectId>,
        in_flight: &HashMap<ProjectId, CancellationToken>,
        regained_focus: bool,
        now: Instant,
    ) -> Option<ProjectId> {
        if let Some(selected) = selected.filter(|project| !in_flight.contains_key(project))
            && (regained_focus || self.due(selected, SELECTED_PROJECT_INTERVAL, now))
        {
            return Some(selected);
        }
        if !in_flight.is_empty() {
            return None;
        }
        others
            .into_iter()
            .filter(|&project| {
                Some(project) != selected && self.due(project, BACKGROUND_PROJECT_INTERVAL, now)
            })
            .min_by_key(|project| {
                (
                    !self.stale.contains(project),
                    self.read.get(project).copied(),
                )
            })
    }
}

impl State {
    /// Something that usually changes a project's pull requests happened, such
    /// as an agent finishing a turn, so its rows are read sooner.
    pub(super) fn mark_stale(&mut self, project: ProjectId) {
        self.schedule.mark_stale(project);
    }

    #[cfg(test)]
    pub(super) fn is_stale(&self, project: ProjectId) -> bool {
        self.schedule.stale.contains(&project)
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
pub enum Action {
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
        let projects = &self.sidebar_model.projects;
        self.refresh_ui
            .schedule
            .retain(|id| projects.iter().any(|project| project.id == id));
        // A project whose checkout could not be read keeps its explicit retry
        // rather than failing again in the background, unless it is selected.
        if let Some((id, path)) = self
            .refresh_ui
            .schedule
            .next(
                sidebar::selected_project(&self.sidebar_model, &self.sidebar_state)
                    .map(|project| project.id),
                projects
                    .iter()
                    .filter(|project| project.unreachable.is_none())
                    .map(|project| project.id),
                &self.refresh_ui.projects,
                regained_focus,
                now,
            )
            .and_then(|id| self.sidebar_model.project(id))
            .map(|project| (project.id, project.repository.clone()))
        {
            tasks.push(self.repository_load_task(path, Some(id), false));
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
        self.refresh_ui.schedule.started(project_id, Instant::now());
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

#[cfg(test)]
mod tests {
    use super::*;

    fn projects<const N: usize>() -> [ProjectId; N] {
        std::array::from_fn(|_| ProjectId::new())
    }

    #[test]
    fn the_selected_project_keeps_its_own_cadence() {
        let [selected, other] = projects();
        let start = Instant::now();
        let mut schedule = ProjectSchedule::default();
        schedule.started(selected, start);
        schedule.started(other, start);
        let in_flight = HashMap::new();
        let next = |schedule: &ProjectSchedule, regained_focus, elapsed| {
            schedule.next(
                Some(selected),
                [selected, other],
                &in_flight,
                regained_focus,
                start + elapsed,
            )
        };
        assert_eq!(next(&schedule, false, Duration::from_secs(30)), None);
        assert_eq!(
            next(&schedule, true, Duration::from_secs(30)),
            Some(selected)
        );
        assert_eq!(
            next(&schedule, false, SELECTED_PROJECT_INTERVAL),
            Some(selected)
        );
        schedule.started(selected, start + SELECTED_PROJECT_INTERVAL);
        assert_eq!(next(&schedule, false, SELECTED_PROJECT_INTERVAL), None);
        // Every other project is read too, once its longer interval is up.
        schedule.started(selected, start + BACKGROUND_PROJECT_INTERVAL);
        assert_eq!(
            next(&schedule, false, BACKGROUND_PROJECT_INTERVAL),
            Some(other)
        );
    }

    #[test]
    fn other_projects_are_read_one_at_a_time_oldest_first() {
        let [selected, older, newer, unread] = projects();
        let start = Instant::now();
        let mut schedule = ProjectSchedule::default();
        schedule.started(selected, start + BACKGROUND_PROJECT_INTERVAL);
        schedule.started(older, start);
        schedule.started(newer, start + Duration::from_secs(10));
        let now = start + BACKGROUND_PROJECT_INTERVAL + Duration::from_secs(10);
        let others = [selected, newer, older];
        assert_eq!(
            schedule.next(Some(selected), others, &HashMap::new(), false, now),
            Some(older)
        );
        // A project never read goes before any that was.
        assert_eq!(
            schedule.next(
                Some(selected),
                [selected, newer, older, unread],
                &HashMap::new(),
                false,
                now
            ),
            Some(unread)
        );
        // Nothing else starts while a read is in flight.
        let in_flight = HashMap::from([(older, CancellationToken::default())]);
        assert_eq!(
            schedule.next(Some(selected), others, &in_flight, false, now),
            None
        );
    }

    #[test]
    fn a_stale_project_is_read_before_its_interval_is_up() {
        let [selected, other, stale] = projects();
        let start = Instant::now();
        let mut schedule = ProjectSchedule::default();
        for project in [selected, other, stale] {
            schedule.started(project, start);
        }
        schedule.mark_stale(stale);
        // Stale shortens the wait rather than skipping it.
        assert_eq!(
            schedule.next(
                Some(selected),
                [selected, other, stale],
                &HashMap::new(),
                false,
                start + Duration::from_secs(10)
            ),
            None
        );
        let now = start + STALE_PROJECT_INTERVAL;
        assert_eq!(
            schedule.next(
                Some(selected),
                [selected, other, stale],
                &HashMap::new(),
                false,
                now
            ),
            Some(stale)
        );
        schedule.started(stale, now);
        assert_eq!(
            schedule.next(
                Some(selected),
                [selected, other, stale],
                &HashMap::new(),
                false,
                now
            ),
            None
        );
        // The selected project is read first when it is the stale one.
        schedule.mark_stale(selected);
        let in_flight = HashMap::from([(other, CancellationToken::default())]);
        assert_eq!(
            schedule.next(Some(selected), [selected], &in_flight, false, now),
            Some(selected)
        );
    }

    #[test]
    fn closed_projects_leave_the_schedule() {
        let [open, closed] = projects();
        let mut schedule = ProjectSchedule::default();
        schedule.started(open, Instant::now());
        schedule.started(closed, Instant::now());
        schedule.mark_stale(closed);
        schedule.retain(|project| project == open);
        assert_eq!(schedule.read.keys().collect::<Vec<_>>(), [&open]);
        assert!(schedule.stale.is_empty());
    }
}
