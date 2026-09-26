use super::*;

/// How long the selected project's rows may go unread while the window is
/// focused.
const SELECTED_PROJECT_INTERVAL: Duration = Duration::from_secs(60);
/// Every other project's rows draw the same pull request and check state, so
/// they are read as well, one at a time. A project with a pull request still
/// open can see its checks or review state change at any moment.
const ACTIVE_PROJECT_INTERVAL: Duration = Duration::from_secs(120);
/// A project with no pull request open changes when someone opens one, which
/// an agent finishing its turn there already brings forward by marking it
/// stale.
const IDLE_PROJECT_INTERVAL: Duration = Duration::from_secs(600);
/// No project is read more often than this, however often an agent finishes
/// a turn in it or the window regains focus.
const MIN_PROJECT_INTERVAL: Duration = Duration::from_secs(30);
/// Reads that keep failing, a GitHub rate limit included, wait twice as long
/// after each failure, up to this.
const MAX_PROJECT_BACKOFF: Duration = Duration::from_secs(30 * 60);

pub(super) struct State {
    pub(super) git: git_ui::State,
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
            git: git_ui::State::default(),
            focused: false,
            source_generation: 0,
            source_cancel: None,
            projects: HashMap::new(),
            schedule: ProjectSchedule::default(),
            last_pull_request: Instant::now(),
        }
    }
}

/// When each project was last read, which ones something has made worth
/// reading before their interval is up, and how many reads in a row failed.
#[derive(Debug, Default)]
struct ProjectSchedule {
    read: HashMap<ProjectId, Instant>,
    stale: HashSet<ProjectId>,
    failures: HashMap<ProjectId, u32>,
}

impl ProjectSchedule {
    fn started(&mut self, project: ProjectId, now: Instant) {
        self.read.insert(project, now);
        self.stale.remove(&project);
    }

    fn finished(&mut self, project: ProjectId, succeeded: bool) {
        if succeeded {
            self.failures.remove(&project);
        } else {
            *self.failures.entry(project).or_default() += 1;
        }
    }

    fn retain(&mut self, open: impl Fn(ProjectId) -> bool) {
        self.read.retain(|&project, _| open(project));
        self.stale.retain(|&project| open(project));
        self.failures.retain(|&project, _| open(project));
    }

    fn mark_stale(&mut self, project: ProjectId) {
        self.stale.insert(project);
    }

    /// How long `project` waits after its last read. Stale and regained
    /// focus shorten the wait to the floor; neither shortens a backoff.
    fn wait(&self, project: ProjectId, interval: Duration, eager: bool) -> Duration {
        match self.failures.get(&project) {
            Some(&failures) => interval
                .saturating_mul(1 << failures.min(8))
                .min(MAX_PROJECT_BACKOFF),
            None if eager || self.stale.contains(&project) => interval.min(MIN_PROJECT_INTERVAL),
            None => interval,
        }
    }

    fn due(&self, project: ProjectId, interval: Duration, eager: bool, now: Instant) -> bool {
        self.read
            .get(&project)
            .is_none_or(|&read| now.duration_since(read) >= self.wait(project, interval, eager))
    }

    /// The one project a refresh tick reads, if any. The selected project
    /// keeps its own cadence; the rest, each with its own interval, wait
    /// until no read is in flight, so a long project list costs one `git`
    /// and `gh` round at a time rather than a burst. The one read longest ago
    /// goes first, and a stale one only breaks a tie, so a project that keeps
    /// going stale cannot hold the others back.
    fn next(
        &self,
        selected: Option<ProjectId>,
        others: impl IntoIterator<Item = (ProjectId, Duration)>,
        in_flight: &HashMap<ProjectId, CancellationToken>,
        regained_focus: bool,
        now: Instant,
    ) -> Option<ProjectId> {
        if let Some(selected) = selected.filter(|project| !in_flight.contains_key(project))
            && self.due(selected, SELECTED_PROJECT_INTERVAL, regained_focus, now)
        {
            return Some(selected);
        }
        if !in_flight.is_empty() {
            return None;
        }
        others
            .into_iter()
            .filter(|&(project, interval)| {
                Some(project) != selected && self.due(project, interval, false, now)
            })
            .map(|(project, _)| project)
            .min_by_key(|project| (self.read.get(project), !self.stale.contains(project)))
    }
}

impl State {
    pub(super) fn started(&mut self, project: ProjectId) {
        self.schedule.started(project, Instant::now());
    }
    /// Something that usually changes a project's pull requests happened, such
    /// as an agent finishing a turn, so its rows are read sooner.
    pub(super) fn mark_stale(&mut self, project: ProjectId) {
        self.schedule.mark_stale(project);
    }

    /// A read finished. One that failed, or reached GitHub only to be
    /// refused, makes the project wait longer before the next.
    pub(super) fn finished(&mut self, project: ProjectId, succeeded: bool) {
        self.schedule.finished(project, succeeded);
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
                self.poll_terminal_agent_activity(None);
                true
            }
            Action::Tick => false,
        };
        if !self.refresh_ui.focused || self.modal_is_open() || self.update_prepared {
            return Task::none();
        }
        let mut tasks = Vec::new();
        tasks.push(self.git_fallback_tick());
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
        self.refresh_ui.projects.retain(|id, cancellation| {
            self.repository_generations.contains_key(id) && !cancellation.is_cancelled()
        });
        // A picker read has a provisional ID until discovery identifies its
        // repository. Prioritize that explicit intent: a focus-regain poll of
        // the existing row must not supersede its listing under another ID.
        // Only live reads block polling; failed, cancelled, and retired opens
        // cannot leave automatic refresh waiting for an acknowledgement.
        if self
            .refresh_ui
            .projects
            .keys()
            .any(|id| self.project_ui.reopening.contains_key(id))
        {
            return Task::batch(tasks);
        }
        // A project whose checkout could not be read is retried from its row
        // rather than in the background, unless it is selected.
        if let Some(id) = self
            .refresh_ui
            .schedule
            .next(
                sidebar::selected_project(&self.sidebar_model, &self.sidebar_state)
                    .filter(|project| project.kind == ProjectKind::Git)
                    .map(|project| project.id),
                projects
                    .iter()
                    .filter(|project| {
                        project.kind == ProjectKind::Git && project.unreachable.is_none()
                    })
                    .map(|project| {
                        let interval = if project.has_open_pull_request() {
                            ACTIVE_PROJECT_INTERVAL
                        } else {
                            IDLE_PROJECT_INTERVAL
                        };
                        (project.id, interval)
                    }),
                &self.refresh_ui.projects,
                regained_focus,
                now,
            )
            .and_then(|id| self.sidebar_model.project(id))
            .map(|project| project.id)
        {
            tasks.push(self.github_read_task(id));
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
        kind: Option<ProjectKind>,
    ) -> Task<Message> {
        self.refresh_ui.schedule.started(project_id, Instant::now());
        self.refresh_ui.git.forget(project_id);
        let cancellation = CancellationToken::default();
        if let Some(previous) = self
            .refresh_ui
            .projects
            .insert(project_id, cancellation.clone())
        {
            previous.cancel();
        }
        let canonical = path.canonicalize().unwrap_or_else(|_| path.clone());
        let kind = kind
            .or_else(|| {
                self.sidebar_model
                    .projects
                    .iter()
                    .find(|project| project.id == project_id || project.repository == canonical)
                    .map(|project| project.kind)
            })
            .or_else(|| {
                self.workspace_model
                    .project(&canonical)
                    .map(|project| project.kind)
            });
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || {
                load_project(&path, Some(project_id), cancellation, kind)
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

    fn active<const N: usize>(projects: [ProjectId; N]) -> [(ProjectId, Duration); N] {
        projects.map(|project| (project, ACTIVE_PROJECT_INTERVAL))
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
                active([selected, other]),
                &in_flight,
                regained_focus,
                start + elapsed,
            )
        };
        assert_eq!(next(&schedule, false, MIN_PROJECT_INTERVAL), None);
        // Regaining focus reads it early, but never twice within the floor.
        assert_eq!(next(&schedule, true, Duration::from_secs(10)), None);
        assert_eq!(next(&schedule, true, MIN_PROJECT_INTERVAL), Some(selected));
        assert_eq!(
            next(&schedule, false, SELECTED_PROJECT_INTERVAL),
            Some(selected)
        );
        schedule.started(selected, start + SELECTED_PROJECT_INTERVAL);
        assert_eq!(next(&schedule, false, SELECTED_PROJECT_INTERVAL), None);
        // Every other project is read too, once its longer interval is up.
        schedule.started(selected, start + ACTIVE_PROJECT_INTERVAL);
        assert_eq!(next(&schedule, false, ACTIVE_PROJECT_INTERVAL), Some(other));
    }

    #[test]
    fn other_projects_are_read_one_at_a_time_oldest_first() {
        let [selected, older, newer, unread] = projects();
        let start = Instant::now();
        let mut schedule = ProjectSchedule::default();
        schedule.started(selected, start + ACTIVE_PROJECT_INTERVAL);
        schedule.started(older, start);
        schedule.started(newer, start + Duration::from_secs(10));
        let now = start + ACTIVE_PROJECT_INTERVAL + Duration::from_secs(10);
        let others = active([selected, newer, older]);
        assert_eq!(
            schedule.next(Some(selected), others, &HashMap::new(), false, now),
            Some(older)
        );
        // A project never read goes before any that was.
        assert_eq!(
            schedule.next(
                Some(selected),
                active([selected, newer, older, unread]),
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
    fn a_project_without_an_open_pull_request_is_read_least_often() {
        let [idle, open] = projects();
        let start = Instant::now();
        let mut schedule = ProjectSchedule::default();
        schedule.started(idle, start);
        schedule.started(open, start);
        let others = [
            (idle, IDLE_PROJECT_INTERVAL),
            (open, ACTIVE_PROJECT_INTERVAL),
        ];
        let next = |schedule: &ProjectSchedule, elapsed| {
            schedule.next(None, others, &HashMap::new(), false, start + elapsed)
        };
        assert_eq!(next(&schedule, ACTIVE_PROJECT_INTERVAL), Some(open));
        schedule.started(open, start + ACTIVE_PROJECT_INTERVAL);
        assert_eq!(next(&schedule, ACTIVE_PROJECT_INTERVAL), None);
        assert_eq!(next(&schedule, IDLE_PROJECT_INTERVAL), Some(idle));
    }

    #[test]
    fn a_stale_project_is_read_early_but_not_ahead_of_older_reads() {
        let [selected, other, stale] = projects();
        let start = Instant::now();
        let mut schedule = ProjectSchedule::default();
        for project in [selected, other, stale] {
            schedule.started(project, start);
        }
        schedule.mark_stale(stale);
        let next = |schedule: &ProjectSchedule, others: &[ProjectId], in_flight, now| {
            schedule.next(
                Some(selected),
                others
                    .iter()
                    .map(|&project| (project, ACTIVE_PROJECT_INTERVAL)),
                in_flight,
                false,
                now,
            )
        };
        let idle = HashMap::new();
        // Stale shortens the wait to the floor rather than skipping it.
        assert_eq!(
            next(
                &schedule,
                &[selected, other, stale],
                &idle,
                start + Duration::from_secs(10)
            ),
            None
        );
        let now = start + MIN_PROJECT_INTERVAL;
        assert_eq!(
            next(&schedule, &[selected, other, stale], &idle, now),
            Some(stale)
        );
        schedule.started(stale, now);
        assert_eq!(next(&schedule, &[selected, other, stale], &idle, now), None);
        // Going stale again cannot put it ahead of a project read longer ago.
        schedule.mark_stale(stale);
        let later = start + ACTIVE_PROJECT_INTERVAL;
        schedule.started(selected, later);
        assert_eq!(
            next(&schedule, &[selected, other, stale], &idle, later),
            Some(other)
        );
        // Among reads started together, the stale one goes first.
        schedule.started(other, now);
        assert_eq!(
            next(&schedule, &[selected, other, stale], &idle, later),
            Some(stale)
        );
        // The selected project is read first when it is the stale one.
        schedule.mark_stale(selected);
        let in_flight = HashMap::from([(other, CancellationToken::default())]);
        assert_eq!(
            next(
                &schedule,
                &[selected],
                &in_flight,
                later + MIN_PROJECT_INTERVAL
            ),
            Some(selected)
        );
    }

    #[test]
    fn failed_reads_back_off_until_one_succeeds() {
        let [project] = projects();
        let start = Instant::now();
        let mut schedule = ProjectSchedule::default();
        let next = |schedule: &ProjectSchedule, regained_focus, elapsed| {
            schedule.next(
                Some(project),
                [],
                &HashMap::new(),
                regained_focus,
                start + elapsed,
            )
        };
        schedule.started(project, start);
        schedule.finished(project, false);
        schedule.mark_stale(project);
        // Neither stale nor focus shortens a backoff.
        assert_eq!(next(&schedule, true, SELECTED_PROJECT_INTERVAL), None);
        assert_eq!(
            next(&schedule, false, SELECTED_PROJECT_INTERVAL * 2),
            Some(project)
        );
        schedule.finished(project, false);
        assert_eq!(next(&schedule, false, SELECTED_PROJECT_INTERVAL * 2), None);
        assert_eq!(
            next(&schedule, false, SELECTED_PROJECT_INTERVAL * 4),
            Some(project)
        );
        for _ in 0..40 {
            schedule.finished(project, false);
        }
        assert_eq!(next(&schedule, false, MAX_PROJECT_BACKOFF), Some(project));
        schedule.finished(project, true);
        assert_eq!(
            next(&schedule, false, SELECTED_PROJECT_INTERVAL),
            Some(project)
        );
    }

    #[test]
    fn closed_projects_leave_the_schedule() {
        let [open, closed] = projects();
        let mut schedule = ProjectSchedule::default();
        schedule.started(open, Instant::now());
        schedule.started(closed, Instant::now());
        schedule.mark_stale(closed);
        schedule.finished(closed, false);
        schedule.retain(|project| project == open);
        assert_eq!(schedule.read.keys().collect::<Vec<_>>(), [&open]);
        assert!(schedule.stale.is_empty());
        assert!(schedule.failures.is_empty());
    }
}
